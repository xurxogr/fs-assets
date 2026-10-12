//! Conversion of raw `$type`-tagged JSON structures into simple values.
//!
//! This operates on the raw asset dump's shaped JSON (`$type`-tagged dicts) — the format
//! this crate's eventual richer asset-extraction pass will need to produce —
//! NOT on `unreal_asset::properties::Property`, the already-parsed-asset
//! representation `convert::uasset_to_json` walks today. See
//! `convert::property_to_value` (private fn in `src/convert/mod.rs`) for the
//! equivalent logic against that representation; the two will need
//! reconciling once the richer extraction JSON shape is designed. This is a
//! parallel, independent implementation, not a refactor of
//! `property_to_value`.

use serde_json::{json, Map, Value};

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn display_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Process any value, converting `$type` structures to simple values.
///
/// This is the main dispatcher that routes to specific type handlers.
pub fn process_value(data: &Value) -> Value {
    match data {
        Value::Array(items) => Value::Array(items.iter().map(process_value).collect()),
        Value::Object(map) => process_object(map),
        other => other.clone(),
    }
}

fn process_object(map: &Map<String, Value>) -> Value {
    let type_str = map.get("$type").and_then(|v| v.as_str()).unwrap_or("");

    if type_str.is_empty() {
        let result: Map<String, Value> = map
            .iter()
            .map(|(k, v)| (k.clone(), process_value(v)))
            .collect();
        return Value::Object(result);
    }

    if type_str.contains("PropertyData") {
        return if type_str.contains("Struct") {
            process_struct_data(map, type_str)
        } else {
            process_property_data(map, type_str)
        };
    }

    if type_str.contains("UnrealTypes") {
        return process_unreal_type(map);
    }

    if type_str.contains("FieldTypes") {
        return process_field_type(map);
    }

    if type_str.contains("CustomVersion") {
        return process_custom_version(map);
    }

    match map.get("Value") {
        Some(v) => v.clone(),
        None => Value::Object(map.clone()),
    }
}

/// Process PropertyData types: `IntPropertyData`, `BoolPropertyData`,
/// `StrPropertyData`, etc.
fn process_property_data(data: &Map<String, Value>, type_str: &str) -> Value {
    // Integer types (note: unlike utils.rs's extract_property_value, this
    // list does NOT include "Int32PropertyData" — "IntPropertyData" is not
    // a substring of "Int32PropertyData", so Int32 props deliberately fall
    // through to the default branch
    // at the bottom of this function instead of being treated as numeric).
    if [
        "IntPropertyData",
        "Int8PropertyData",
        "Int16PropertyData",
        "Int64PropertyData",
        "UInt16PropertyData",
        "UInt32PropertyData",
    ]
    .iter()
    .any(|m| type_str.contains(m))
    {
        return data.get("Value").cloned().unwrap_or(json!(0));
    }

    if type_str.contains("BoolPropertyData") {
        return data.get("Value").cloned().unwrap_or(json!(false));
    }

    if type_str.contains("FloatPropertyData") {
        return data.get("Value").cloned().unwrap_or(json!(0.0));
    }

    if type_str.contains("StrPropertyData") || type_str.contains("NamePropertyData") {
        return match data.get("Value") {
            Some(Value::Object(m)) => m.get("Value").cloned().unwrap_or(json!("")),
            Some(v) if is_truthy(v) => json!(display_string(v)),
            _ => json!(""),
        };
    }

    if type_str.contains("TextPropertyData") {
        let culture_invariant = data.get("CultureInvariantString");
        let value = data.get("Value");
        let ci_truthy = culture_invariant.map(is_truthy).unwrap_or(false);
        let value_truthy = value.map(is_truthy).unwrap_or(false);
        let value_is_str = matches!(value, Some(Value::String(_)));

        if ci_truthy && value_truthy && value_is_str {
            return json!({"Text": culture_invariant.unwrap().clone(), "Guid": value.unwrap().clone()});
        }
        if ci_truthy {
            return culture_invariant.unwrap().clone();
        }
        return match value {
            Some(Value::Object(m)) => m
                .get("CultureInvariantString")
                .cloned()
                .unwrap_or(json!("")),
            Some(v) if is_truthy(v) => json!(display_string(v)),
            _ => json!(""),
        };
    }

    if type_str.contains("BytePropertyData") || type_str.contains("EnumPropertyData") {
        return match data.get("Value") {
            Some(Value::Object(m)) => m.get("Value").cloned().unwrap_or(json!("")),
            Some(v) => v.clone(),
            None => Value::Null,
        };
    }

    // SoftObject (checked before ObjectPropertyData since it's a substring match).
    if type_str.contains("SoftObjectPropertyData") {
        return match data.get("Value") {
            Some(Value::Object(m)) => m.get("AssetPathName").cloned().unwrap_or(json!("")),
            Some(v) => v.clone(),
            None => Value::Null,
        };
    }

    if type_str.contains("ObjectPropertyData") {
        return match data.get("Value") {
            Some(Value::Object(m)) => {
                let index = m.get("Index").cloned().unwrap_or(json!(0));
                json!(format!("Reference: {index}"))
            }
            Some(v @ Value::Number(n)) if n.is_i64() || n.is_u64() => {
                json!(format!("Reference: {v}"))
            }
            Some(v) => v.clone(),
            None => Value::Null,
        };
    }

    if type_str.contains("ArrayPropertyData") {
        let value = data.get("Value").cloned().unwrap_or(json!([]));
        return match &value {
            Value::Array(items) => Value::Array(items.iter().map(process_value).collect()),
            other => other.clone(),
        };
    }

    if type_str.contains("MapPropertyData") {
        return data.get("Value").cloned().unwrap_or(json!({}));
    }

    if type_str.contains("SetPropertyData") {
        let value = data.get("Value").cloned().unwrap_or(json!([]));
        return match &value {
            Value::Array(items) => Value::Array(items.iter().map(process_value).collect()),
            other => other.clone(),
        };
    }

    if type_str.contains("FDelegate") || type_str.contains("DelegatePropertyData") {
        return data.get("Value").cloned().unwrap_or(Value::Null);
    }

    if type_str.contains("MulticastSparseDelegatePropertyData") {
        return data.get("Value").cloned().unwrap_or(json!([]));
    }

    data.get("Value").cloned().unwrap_or(Value::Null)
}

/// Process Struct types: `VectorPropertyData`, `ColorPropertyData`,
/// `StructPropertyData`, etc.
fn process_struct_data(data: &Map<String, Value>, type_str: &str) -> Value {
    let value = data.get("Value").cloned().unwrap_or(json!([]));

    const EXCLUDED: &[&str] = &[
        "Vector",
        "Color",
        "Rotator",
        "Quat",
        "Box",
        "IntPoint",
        "Guid",
        "RichCurve",
    ];

    if type_str.contains("StructPropertyData") && !EXCLUDED.iter().any(|x| type_str.contains(x)) {
        return match &value {
            Value::Array(items) => {
                let mut result = Map::new();
                for prop in items {
                    if let Value::Object(m) = prop {
                        if let Some(name) = m.get("Name").and_then(|v| v.as_str()) {
                            result.insert(name.to_string(), process_value(prop));
                        }
                    }
                }
                Value::Object(result)
            }
            other => other.clone(),
        };
    }

    if type_str.contains("GuidPropertyData") {
        return match &value {
            Value::Object(m) => json!(display_string(&m.get("Guid").cloned().unwrap_or(json!("")))),
            other => other.clone(),
        };
    }

    if type_str.contains("PerPlatformFloatPropertyData") {
        return data.get("Value").cloned().unwrap_or(json!(0.0));
    }

    // All other struct types return value as-is: Vector types (Vector2D,
    // Vector, Vector4), Rotator, Quat, Color types, Box types, IntPoint,
    // RichCurveKey, Material/Expression Input, SkeletalMesh.
    value
}

/// Process UnrealTypes: `FVector`, `FRotator`, `FLinearColor`, etc.
///
/// These are usually simple structs, returned without the `$type` field
/// (shallow only — nested values are not recursively processed).
fn process_unreal_type(data: &Map<String, Value>) -> Value {
    let result: Map<String, Value> = data
        .iter()
        .filter(|(k, _)| *k != "$type")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(result)
}

/// Process Field types: `UArrayProperty`, `UBoolProperty`, `UObjectProperty`, etc.
///
/// Field types describe property metadata, not values; returns a simplified
/// version.
fn process_field_type(data: &Map<String, Value>) -> Value {
    json!({
        "Name": data.get("Name").cloned().unwrap_or(json!("")),
        "Flags": data.get("Flags").cloned().unwrap_or(json!(0)),
    })
}

/// Process CustomVersion: converts to a simple `{FriendlyName: Version}` mapping.
fn process_custom_version(data: &Map<String, Value>) -> Value {
    let friendly_name = data
        .get("FriendlyName")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let version = data.get("Version").cloned().unwrap_or(json!(0));
    let mut result = Map::new();
    result.insert(friendly_name, version);
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod basics {
        use super::*;

        #[test]
        fn list_processes_each_item() {
            let data = json!([
                {"$type": "IntPropertyData", "Value": 1},
                {"$type": "IntPropertyData", "Value": 2},
            ]);
            assert_eq!(process_value(&data), json!([1, 2]));
        }

        #[test]
        fn non_dict_primitive_returned_as_is() {
            assert_eq!(process_value(&json!(5)), json!(5));
            assert_eq!(process_value(&json!("foo")), json!("foo"));
            assert_eq!(process_value(&json!(true)), json!(true));
            assert_eq!(process_value(&Value::Null), Value::Null);
        }

        #[test]
        fn dict_without_type_processed_recursively() {
            let data = json!({"a": {"$type": "IntPropertyData", "Value": 1}, "b": 2});
            assert_eq!(process_value(&data), json!({"a": 1, "b": 2}));
        }

        #[test]
        fn unknown_type_returns_value_field() {
            assert_eq!(
                process_value(&json!({"$type": "SomethingUnknown", "Value": "raw"})),
                json!("raw")
            );
        }

        #[test]
        fn unknown_type_without_value_returns_whole_dict() {
            let data = json!({"$type": "SomethingUnknown", "Other": 1});
            assert_eq!(process_value(&data), data);
        }
    }

    mod property_data {
        use super::*;

        #[test]
        fn int_property_data() {
            assert_eq!(
                process_value(&json!({"$type": "IntPropertyData", "Value": 42})),
                json!(42)
            );
            assert_eq!(
                process_value(&json!({"$type": "Int64PropertyData", "Value": 42})),
                json!(42)
            );
            assert_eq!(
                process_value(&json!({"$type": "UInt16PropertyData"})),
                json!(0)
            );
        }

        #[test]
        fn bool_property_data() {
            assert_eq!(
                process_value(&json!({"$type": "BoolPropertyData", "Value": true})),
                json!(true)
            );
            assert_eq!(
                process_value(&json!({"$type": "BoolPropertyData"})),
                json!(false)
            );
        }

        #[test]
        fn float_property_data() {
            assert_eq!(
                process_value(&json!({"$type": "FloatPropertyData", "Value": 1.5})),
                json!(1.5)
            );
            assert_eq!(
                process_value(&json!({"$type": "FloatPropertyData"})),
                json!(0.0)
            );
        }

        #[test]
        fn str_property_data_string_value() {
            assert_eq!(
                process_value(&json!({"$type": "StrPropertyData", "Value": "hi"})),
                json!("hi")
            );
        }

        #[test]
        fn str_property_data_dict_value() {
            let data = json!({"$type": "NamePropertyData", "Value": {"Value": "nested"}});
            assert_eq!(process_value(&data), json!("nested"));
        }

        #[test]
        fn str_property_data_falsy_value() {
            assert_eq!(
                process_value(&json!({"$type": "StrPropertyData", "Value": null})),
                json!("")
            );
        }

        #[test]
        fn text_property_data_with_culture_and_guid() {
            let data = json!({
                "$type": "TextPropertyData",
                "CultureInvariantString": "Hello",
                "Value": "abc-guid",
            });
            assert_eq!(
                process_value(&data),
                json!({"Text": "Hello", "Guid": "abc-guid"})
            );
        }

        #[test]
        fn text_property_data_culture_only() {
            let data = json!({"$type": "TextPropertyData", "CultureInvariantString": "Hello"});
            assert_eq!(process_value(&data), json!("Hello"));
        }

        #[test]
        fn text_property_data_dict_value_fallback() {
            let data =
                json!({"$type": "TextPropertyData", "Value": {"CultureInvariantString": "Nested"}});
            assert_eq!(process_value(&data), json!("Nested"));
        }

        #[test]
        fn text_property_data_string_value_fallback() {
            let data = json!({"$type": "TextPropertyData", "Value": "raw-guid"});
            assert_eq!(process_value(&data), json!("raw-guid"));
        }

        #[test]
        fn text_property_data_no_text_at_all() {
            assert_eq!(
                process_value(&json!({"$type": "TextPropertyData"})),
                json!("")
            );
        }

        #[test]
        fn byte_and_enum_property_data() {
            assert_eq!(
                process_value(&json!({"$type": "EnumPropertyData", "Value": {"Value": "Foo"}})),
                json!("Foo")
            );
            assert_eq!(
                process_value(&json!({"$type": "BytePropertyData", "Value": 3})),
                json!(3)
            );
        }

        #[test]
        fn soft_object_property_data() {
            let data =
                json!({"$type": "SoftObjectPropertyData", "Value": {"AssetPathName": "/Game/Foo"}});
            assert_eq!(process_value(&data), json!("/Game/Foo"));
            assert_eq!(
                process_value(&json!({"$type": "SoftObjectPropertyData", "Value": "raw"})),
                json!("raw")
            );
        }

        #[test]
        fn object_property_data_dict_index() {
            let data = json!({"$type": "ObjectPropertyData", "Value": {"Index": 7}});
            assert_eq!(process_value(&data), json!("Reference: 7"));
        }

        #[test]
        fn object_property_data_int_index() {
            let data = json!({"$type": "ObjectPropertyData", "Value": 9});
            assert_eq!(process_value(&data), json!("Reference: 9"));
        }

        #[test]
        fn object_property_data_other_value() {
            assert_eq!(
                process_value(&json!({"$type": "ObjectPropertyData", "Value": null})),
                Value::Null
            );
        }

        #[test]
        fn array_property_data() {
            let data = json!({
                "$type": "ArrayPropertyData",
                "Value": [{"$type": "IntPropertyData", "Value": 1}],
            });
            assert_eq!(process_value(&data), json!([1]));
        }

        #[test]
        fn array_property_data_non_list_value() {
            assert_eq!(
                process_value(&json!({"$type": "ArrayPropertyData", "Value": null})),
                Value::Null
            );
        }

        #[test]
        fn map_property_data() {
            let data = json!({"$type": "MapPropertyData", "Value": {"a": 1}});
            assert_eq!(process_value(&data), json!({"a": 1}));
        }

        #[test]
        fn set_property_data() {
            let data = json!({"$type": "SetPropertyData", "Value": [{"$type": "IntPropertyData", "Value": 1}]});
            assert_eq!(process_value(&data), json!([1]));
        }

        #[test]
        fn set_property_data_non_list_value() {
            assert_eq!(
                process_value(&json!({"$type": "SetPropertyData", "Value": null})),
                Value::Null
            );
        }

        #[test]
        fn delegate_property_data() {
            assert_eq!(
                process_value(&json!({"$type": "FDelegateProperty", "Value": "cb"})),
                json!("cb")
            );
            assert_eq!(
                process_value(&json!({"$type": "DelegatePropertyData", "Value": "cb"})),
                json!("cb")
            );
        }

        #[test]
        fn multicast_sparse_delegate_property_data() {
            // MulticastSparseDelegatePropertyData is caught by the earlier
            // Delegate check ("MulticastSparseDelegatePropertyData" contains
            // "DelegatePropertyData" as a substring), so the generic Delegate
            // branch matches first (no default), and the dedicated Multicast
            // branch below it is unreachable.
            assert_eq!(
                process_value(&json!({"$type": "MulticastSparseDelegatePropertyData"})),
                Value::Null
            );
        }

        #[test]
        fn default_property_data_fallback() {
            assert_eq!(
                process_value(&json!({"$type": "WeirdPropertyData", "Value": "x"})),
                json!("x")
            );
        }
    }

    mod struct_data {
        use super::*;

        #[test]
        fn struct_property_data_processes_nested_properties() {
            let data = json!({
                "$type": "StructPropertyData",
                "Value": [
                    {"Name": "Foo", "$type": "IntPropertyData", "Value": 1},
                    {"NoName": true},
                ],
            });
            assert_eq!(process_value(&data), json!({"Foo": 1}));
        }

        #[test]
        fn struct_property_data_non_list_value() {
            assert_eq!(
                process_value(&json!({"$type": "StructPropertyData", "Value": "raw"})),
                json!("raw")
            );
        }

        #[test]
        fn vector_struct_returns_value_as_is() {
            let data =
                json!({"$type": "VectorStructPropertyData", "Value": {"X": 1, "Y": 2, "Z": 3}});
            assert_eq!(process_value(&data), json!({"X": 1, "Y": 2, "Z": 3}));
        }

        #[test]
        fn guid_property_data_dict_value() {
            let data = json!({"$type": "StructGuidPropertyData", "Value": {"Guid": "abc-123"}});
            assert_eq!(process_value(&data), json!("abc-123"));
        }

        #[test]
        fn guid_property_data_non_dict_value() {
            let data = json!({"$type": "StructGuidPropertyData", "Value": "already-a-string"});
            assert_eq!(process_value(&data), json!("already-a-string"));
        }

        #[test]
        fn per_platform_float_property_data() {
            let data = json!({"$type": "StructPerPlatformFloatPropertyData", "Value": 2.5});
            assert_eq!(process_value(&data), json!(2.5));
            assert_eq!(
                process_value(&json!({"$type": "StructPerPlatformFloatPropertyData"})),
                json!(0.0)
            );
        }
    }

    mod unreal_type {
        use super::*;

        #[test]
        fn unreal_type_strips_type_key() {
            let data = json!({"$type": "UnrealTypes.FVector", "X": 1, "Y": 2});
            assert_eq!(process_value(&data), json!({"X": 1, "Y": 2}));
        }
    }

    mod field_type {
        use super::*;

        #[test]
        fn field_type_returns_name_and_flags() {
            let data = json!({"$type": "FieldTypes.UBoolProperty", "Name": "bFoo", "Flags": 5, "Other": "x"});
            assert_eq!(process_value(&data), json!({"Name": "bFoo", "Flags": 5}));
        }

        #[test]
        fn field_type_defaults() {
            assert_eq!(
                process_value(&json!({"$type": "FieldTypes.UBoolProperty"})),
                json!({"Name": "", "Flags": 0})
            );
        }
    }

    mod custom_version {
        use super::*;

        #[test]
        fn custom_version_maps_friendly_name_to_version() {
            let data = json!({"$type": "CustomVersion", "FriendlyName": "Core", "Version": 3});
            assert_eq!(process_value(&data), json!({"Core": 3}));
        }

        #[test]
        fn custom_version_defaults() {
            assert_eq!(
                process_value(&json!({"$type": "CustomVersion"})),
                json!({"Unknown": 0})
            );
        }
    }
}
