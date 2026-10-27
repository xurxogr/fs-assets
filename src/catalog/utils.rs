//! Shared utilities for catalog builder services.

use serde_json::{json, Map, Value};

/// Normalize an object path to War/Content format.
///
/// Converts `/Game/` paths to `War/Content/` format and ensures a `.0` suffix.
pub fn normalize_object_path(path: &str) -> String {
    let mut result = if let Some(rest) = path.strip_prefix("/Game/") {
        format!("War/Content/{rest}")
    } else {
        path.to_string()
    };
    if !result.ends_with(".0") {
        result.push_str(".0");
    }
    result
}

/// Extract text from a localized value.
///
/// Handles both dict format `{"Text": "...", "Guid": "..."}` and plain strings.
pub fn extract_localized_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Object(map) => map.get("Text").map(value_to_display_string),
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        other => Some(value_to_display_string(other)),
    }
}

/// Display rendering of a scalar JSON value: strings unquoted, booleans as
/// `True`/`False`, null as `None`.
fn value_to_display_string(value: &Value) -> String {
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

fn extract_numeric(_prop: &Value, value: &Value) -> Value {
    simplify_value(value)
}

fn extract_bool(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Bool(b) => Value::Bool(*b),
        _ => Value::Bool(false),
    }
}

fn extract_raw(_prop: &Value, value: &Value) -> Value {
    value.clone()
}

fn extract_name(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Object(m) => m.get("Value").cloned().unwrap_or_else(|| json!("")),
        other => other.clone(),
    }
}

fn extract_str(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Object(m) => m.get("Value").cloned().unwrap_or_else(|| json!("")),
        Value::Null => json!(""),
        other => json!(value_to_display_string(other)),
    }
}

fn extract_text(prop: &Value, value: &Value) -> Value {
    let culture_invariant = prop.get("CultureInvariantString");
    let ci_truthy = culture_invariant.map(is_truthy).unwrap_or(false);

    if ci_truthy && is_truthy(value) {
        if let Value::String(s) = value {
            return json!({"Text": culture_invariant.unwrap().clone(), "Guid": s});
        }
    }
    if ci_truthy {
        return culture_invariant.unwrap().clone();
    }
    match value {
        Value::Object(m) => m
            .get("CultureInvariantString")
            .cloned()
            .unwrap_or_else(|| json!("")),
        Value::Null => json!(""),
        other => json!(value_to_display_string(other)),
    }
}

fn extract_object_reference(_prop: &Value, value: &Value) -> Value {
    let index = match value {
        Value::Object(m) => m.get("Index").cloned().unwrap_or_else(|| json!(0)),
        Value::Number(n) if n.is_i64() || n.is_u64() => value.clone(),
        _ => return value.clone(),
    };
    json!(format!("Reference: {index}"))
}

fn extract_soft_object(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Object(m) => m.get("AssetPathName").cloned().unwrap_or_else(|| json!("")),
        other => other.clone(),
    }
}

fn extract_collection(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| {
                    if item.get("$type").is_some() {
                        extract_property_value(item)
                    } else {
                        item.clone()
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn extract_struct(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            let mut result = Map::new();
            for inner_prop in items {
                if let Value::Object(m) = inner_prop {
                    if let Some(name) = m.get("Name").and_then(|v| v.as_str()) {
                        let inner_value = extract_property_value(inner_prop);
                        result.insert(name.to_string(), inner_value);
                    }
                }
            }
            simplify_value(&Value::Object(result))
        }
        other => simplify_value(other),
    }
}

fn extract_multicast_delegate(_prop: &Value, value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.clone()),
        _ => Value::Array(Vec::new()),
    }
}

type Handler = fn(&Value, &Value) -> Value;

/// Ordered (type-name markers, handler) pairs, evaluated in order (first match wins).
/// A property matches the first entry whose markers appear anywhere in `$type`.
const PROPERTY_HANDLERS: &[(&[&str], Handler)] = &[
    (
        &[
            "IntPropertyData",
            "Int8PropertyData",
            "Int16PropertyData",
            "Int32PropertyData",
            "Int64PropertyData",
            "UInt16PropertyData",
            "UInt32PropertyData",
        ],
        extract_numeric,
    ),
    (&["FloatPropertyData"], extract_numeric),
    (&["BoolPropertyData"], extract_bool),
    (&["BytePropertyData", "EnumPropertyData"], extract_raw),
    (&["NamePropertyData"], extract_name),
    (&["StrPropertyData"], extract_str),
    (&["TextPropertyData"], extract_text),
    (&["ObjectPropertyData"], extract_object_reference),
    (&["SoftObjectPropertyData"], extract_soft_object),
    (&["ArrayPropertyData"], extract_collection),
    (&["SetPropertyData"], extract_collection),
    (&["MapPropertyData"], extract_raw),
    (&["StructPropertyData"], extract_struct),
    (&["DelegatePropertyData", "FDelegate"], extract_raw),
    (
        &["MulticastSparseDelegatePropertyData"],
        extract_multicast_delegate,
    ),
];

/// Extract the value from a UAsset property dict.
///
/// Centralized extraction for common UAsset property types. `prop` is a
/// `$type`/`Value`-tagged dict; the first marker group (in `PROPERTY_HANDLERS`
/// order) whose name appears anywhere in `$type` wins. Because
/// `ObjectPropertyData`'s entry comes before `SoftObjectPropertyData`'s, and
/// `"ObjectPropertyData"` is a substring of `"SoftObjectPropertyData"`, a
/// `SoftObjectPropertyData` prop is actually handled by the Object branch —
/// this ordering is kept deliberately so existing catalog output stays
/// stable.
pub fn extract_property_value(prop: &Value) -> Value {
    let prop_type = prop.get("$type").and_then(|v| v.as_str()).unwrap_or("");
    let value = prop.get("Value").cloned().unwrap_or(Value::Null);

    for (markers, handler) in PROPERTY_HANDLERS {
        if markers.iter().any(|m| prop_type.contains(m)) {
            return handler(prop, &value);
        }
    }

    value
}

/// Simplify extracted values.
///
/// - Convert floats to ints if they're whole numbers
/// - Convert '+N' strings to ints (game format for numbers)
/// - Flatten single-key dicts to just the value
/// - Recursively simplify nested structures
pub fn simplify_value(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            let rounded = (f * 1_000_000.0).round() / 1_000_000.0;
            if rounded == rounded.trunc() && rounded.is_finite() {
                json!(rounded as i64)
            } else {
                json!(rounded)
            }
        }
        Value::String(s) => {
            if !s.is_empty()
                && (s.starts_with('+') || s.starts_with('-'))
                && s.len() > 1
                && s[1..].chars().all(|c| c.is_ascii_digit())
            {
                json!(s.parse::<i64>().unwrap())
            } else {
                value.clone()
            }
        }
        Value::Object(map) => {
            let simplified: Map<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), simplify_value(v)))
                .collect();
            if simplified.len() == 1 {
                simplified.values().next().unwrap().clone()
            } else {
                Value::Object(simplified)
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(simplify_value).collect()),
        other => other.clone(),
    }
}

/// Parse a reference string like `"Reference: -5"` to extract the index.
///
/// Returns the reference index (negative for imports, positive for
/// exports), or `None` if parsing fails.
pub fn parse_reference(ref_string: &str) -> Option<i64> {
    if ref_string.is_empty() || !ref_string.starts_with("Reference:") {
        return None;
    }
    ref_string
        .split(':')
        .nth(1)
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Resolve an import index to its full object path.
///
/// Follows the `OuterIndex` chain to build the complete path. `import_index`
/// must be a negative import index; `imports` is the raw imports array from
/// JSON.
pub fn resolve_import_path(import_index: i64, imports: &[Value]) -> Option<String> {
    let mut current = import_at(import_index, imports)?;
    let mut names = Vec::new();
    loop {
        names.push(
            current
                .get("ObjectName")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        // A chain longer than the imports table must revisit an entry: an
        // `OuterIndex` cycle in a malformed asset, which would otherwise be
        // followed forever.
        if names.len() > imports.len() {
            return None;
        }
        let outer_index = current
            .get("OuterIndex")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        match import_at(outer_index, imports) {
            Some(outer) => current = outer,
            None => break,
        }
    }
    names.reverse();
    Some(names.join("/"))
}

/// The import a negative `import_index` points at, or `None` for a
/// non-negative or out-of-range index.
fn import_at(import_index: i64, imports: &[Value]) -> Option<&Value> {
    if import_index >= 0 {
        return None;
    }
    let index = usize::try_from(import_index.unsigned_abs() - 1).ok()?;
    imports.get(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod normalize_object_path_tests {
        use super::*;

        #[test]
        fn converts_game_path_to_war_content() {
            assert_eq!(
                normalize_object_path("/Game/Blueprints/Items/BPFoo"),
                "War/Content/Blueprints/Items/BPFoo.0"
            );
        }

        #[test]
        fn adds_dot_zero_suffix() {
            assert_eq!(
                normalize_object_path("War/Content/Items/BPFoo"),
                "War/Content/Items/BPFoo.0"
            );
        }

        #[test]
        fn does_not_duplicate_dot_zero_suffix() {
            assert_eq!(
                normalize_object_path("War/Content/Items/BPFoo.0"),
                "War/Content/Items/BPFoo.0"
            );
        }

        #[test]
        fn handles_nested_paths() {
            assert_eq!(
                normalize_object_path("/Game/Blueprints/Items/Weapons/BPRifle"),
                "War/Content/Blueprints/Items/Weapons/BPRifle.0"
            );
        }

        #[test]
        fn handles_paths_without_game_prefix() {
            assert_eq!(
                normalize_object_path("SomePath/Items/BPFoo"),
                "SomePath/Items/BPFoo.0"
            );
        }
    }

    mod extract_localized_text_tests {
        use super::*;

        #[test]
        fn extracts_text_from_dict() {
            let v = json!({"Text": "Hello World", "Guid": "ABC123"});
            assert_eq!(extract_localized_text(&v), Some("Hello World".to_string()));
        }

        #[test]
        fn returns_string_as_is() {
            let v = json!("Plain text");
            assert_eq!(extract_localized_text(&v), Some("Plain text".to_string()));
        }

        #[test]
        fn returns_none_for_none_value() {
            assert_eq!(extract_localized_text(&Value::Null), None);
        }

        #[test]
        fn returns_none_for_empty_string() {
            assert_eq!(extract_localized_text(&json!("")), None);
        }

        #[test]
        fn returns_none_for_dict_without_text() {
            assert_eq!(extract_localized_text(&json!({"Guid": "ABC123"})), None);
        }

        #[test]
        fn converts_non_string_to_string() {
            assert_eq!(extract_localized_text(&json!(42)), Some("42".to_string()));
        }
    }

    mod simplify_value_tests {
        use super::*;

        #[test]
        fn converts_whole_float_to_int() {
            let result = simplify_value(&json!(5.0));
            assert_eq!(result, json!(5));
            assert!(result.is_i64());
        }

        #[test]
        fn keeps_fractional_float() {
            let result = simplify_value(&json!(5.5));
            assert_eq!(result, json!(5.5));
            assert!(result.is_f64());
        }

        #[test]
        fn rounds_float_to_6_decimals() {
            let result = simplify_value(&json!(1.23456789));
            assert_eq!(result, json!(1.234568));
        }

        #[test]
        fn converts_plus_string_to_int() {
            let result = simplify_value(&json!("+42"));
            assert_eq!(result, json!(42));
            assert!(result.is_i64());
        }

        #[test]
        fn converts_minus_string_to_int() {
            let result = simplify_value(&json!("-10"));
            assert_eq!(result, json!(-10));
            assert!(result.is_i64());
        }

        #[test]
        fn flattens_single_key_dict() {
            let result = simplify_value(&json!({"Value": 42}));
            assert_eq!(result, json!(42));
        }

        #[test]
        fn keeps_multi_key_dict() {
            let result = simplify_value(&json!({"X": 1, "Y": 2}));
            assert_eq!(result, json!({"X": 1, "Y": 2}));
        }

        #[test]
        fn recursively_simplifies_nested_dict() {
            let result = simplify_value(&json!({"Value": {"Inner": 5.0}}));
            assert_eq!(result, json!(5));
        }

        #[test]
        fn recursively_simplifies_list() {
            let result = simplify_value(&json!([5.0, "+10", {"Value": 3.0}]));
            assert_eq!(result, json!([5, 10, 3]));
        }

        #[test]
        fn keeps_regular_string() {
            let result = simplify_value(&json!("hello"));
            assert_eq!(result, json!("hello"));
        }
    }

    mod parse_reference_tests {
        use super::*;

        #[test]
        fn parses_positive_reference() {
            assert_eq!(parse_reference("Reference: 5"), Some(5));
        }

        #[test]
        fn parses_negative_reference() {
            assert_eq!(parse_reference("Reference: -10"), Some(-10));
        }

        #[test]
        fn parses_zero_reference() {
            assert_eq!(parse_reference("Reference: 0"), Some(0));
        }

        #[test]
        fn returns_none_for_invalid_format() {
            assert_eq!(parse_reference("Not a reference"), None);
        }

        #[test]
        fn returns_none_for_empty_string() {
            assert_eq!(parse_reference(""), None);
        }

        #[test]
        fn returns_none_for_malformed_reference() {
            assert_eq!(parse_reference("Reference: abc"), None);
        }
    }

    mod resolve_import_path_tests {
        use super::*;

        #[test]
        fn resolves_simple_import() {
            let imports =
                vec![json!({"ObjectName": "/Game/Blueprints/Items/BPRifle", "OuterIndex": 0})];
            assert_eq!(
                resolve_import_path(-1, &imports),
                Some("/Game/Blueprints/Items/BPRifle".to_string())
            );
        }

        #[test]
        fn resolves_nested_import() {
            let imports = vec![
                json!({"ObjectName": "/Game/Blueprints/Items", "OuterIndex": 0}),
                json!({"ObjectName": "BPRifle", "OuterIndex": -1}),
            ];
            assert_eq!(
                resolve_import_path(-2, &imports),
                Some("/Game/Blueprints/Items/BPRifle".to_string())
            );
        }

        #[test]
        fn returns_none_for_positive_index() {
            let imports = vec![json!({"ObjectName": "Test", "OuterIndex": 0})];
            assert_eq!(resolve_import_path(1, &imports), None);
        }

        #[test]
        fn returns_none_for_zero_index() {
            let imports = vec![json!({"ObjectName": "Test", "OuterIndex": 0})];
            assert_eq!(resolve_import_path(0, &imports), None);
        }

        #[test]
        fn returns_none_for_out_of_bounds_index() {
            let imports = vec![json!({"ObjectName": "Test", "OuterIndex": 0})];
            assert_eq!(resolve_import_path(-5, &imports), None);
        }

        #[test]
        fn handles_deeply_nested_imports() {
            let imports = vec![
                json!({"ObjectName": "/Game", "OuterIndex": 0}),
                json!({"ObjectName": "Blueprints", "OuterIndex": -1}),
                json!({"ObjectName": "Items", "OuterIndex": -2}),
                json!({"ObjectName": "BPRifle", "OuterIndex": -3}),
            ];
            assert_eq!(
                resolve_import_path(-4, &imports),
                Some("/Game/Blueprints/Items/BPRifle".to_string())
            );
        }

        #[test]
        fn returns_object_name_when_outer_path_none() {
            let imports = vec![
                json!({"ObjectName": "Root", "OuterIndex": -99}),
                json!({"ObjectName": "Child", "OuterIndex": -1}),
            ];
            assert_eq!(
                resolve_import_path(-2, &imports),
                Some("Root/Child".to_string())
            );
        }

        #[test]
        fn returns_none_for_self_referencing_outer() {
            let imports = vec![json!({"ObjectName": "Loop", "OuterIndex": -1})];
            assert_eq!(resolve_import_path(-1, &imports), None);
        }

        #[test]
        fn returns_none_for_outer_cycle() {
            let imports = vec![
                json!({"ObjectName": "A", "OuterIndex": -2}),
                json!({"ObjectName": "B", "OuterIndex": -1}),
                json!({"ObjectName": "C", "OuterIndex": -1}),
            ];
            assert_eq!(resolve_import_path(-3, &imports), None);
        }

        #[test]
        fn returns_none_for_min_index() {
            let imports = vec![json!({"ObjectName": "Test", "OuterIndex": 0})];
            assert_eq!(resolve_import_path(i64::MIN, &imports), None);
        }

        #[test]
        fn returns_object_name_alone_when_no_outer() {
            let imports = vec![json!({"ObjectName": "Standalone", "OuterIndex": 0})];
            assert_eq!(
                resolve_import_path(-1, &imports),
                Some("Standalone".to_string())
            );
        }
    }

    mod extract_property_value_tests {
        use super::*;

        #[test]
        fn extracts_int_property() {
            let prop = json!({"$type": "IntPropertyData", "Value": 42});
            assert_eq!(extract_property_value(&prop), json!(42));
        }

        #[test]
        fn extracts_bool_property_true() {
            let prop = json!({"$type": "BoolPropertyData", "Value": true});
            assert_eq!(extract_property_value(&prop), json!(true));
        }

        #[test]
        fn extracts_bool_property_false() {
            let prop = json!({"$type": "BoolPropertyData", "Value": false});
            assert_eq!(extract_property_value(&prop), json!(false));
        }

        #[test]
        fn extracts_float_property() {
            let prop = json!({"$type": "FloatPropertyData", "Value": 2.5});
            assert_eq!(extract_property_value(&prop), json!(2.5));
        }

        #[test]
        fn extracts_str_property_from_dict() {
            let prop = json!({"$type": "StrPropertyData", "Value": {"Value": "hello"}});
            assert_eq!(extract_property_value(&prop), json!("hello"));
        }

        #[test]
        fn extracts_str_property_direct() {
            let prop = json!({"$type": "StrPropertyData", "Value": "hello"});
            assert_eq!(extract_property_value(&prop), json!("hello"));
        }

        #[test]
        fn extracts_text_property_with_guid() {
            let prop = json!({
                "$type": "TextPropertyData",
                "CultureInvariantString": "Display Text",
                "Value": "ABC123GUID",
            });
            assert_eq!(
                extract_property_value(&prop),
                json!({"Text": "Display Text", "Guid": "ABC123GUID"})
            );
        }

        #[test]
        fn extracts_text_property_without_guid() {
            let prop =
                json!({"$type": "TextPropertyData", "CultureInvariantString": "Display Text"});
            assert_eq!(extract_property_value(&prop), json!("Display Text"));
        }

        #[test]
        fn extracts_object_property() {
            let prop = json!({"$type": "ObjectPropertyData", "Value": {"Index": -5}});
            assert_eq!(extract_property_value(&prop), json!("Reference: -5"));
        }

        #[test]
        fn extracts_object_property_direct_int() {
            let prop = json!({"$type": "ObjectPropertyData", "Value": -5});
            assert_eq!(extract_property_value(&prop), json!("Reference: -5"));
        }

        #[test]
        fn extracts_array_property() {
            let prop = json!({
                "$type": "ArrayPropertyData",
                "Value": [
                    {"$type": "IntPropertyData", "Value": 1},
                    {"$type": "IntPropertyData", "Value": 2},
                ],
            });
            assert_eq!(extract_property_value(&prop), json!([1, 2]));
        }

        #[test]
        fn extracts_enum_property() {
            let prop = json!({"$type": "EnumPropertyData", "Value": "EItemCategory::Weapon"});
            assert_eq!(
                extract_property_value(&prop),
                json!("EItemCategory::Weapon")
            );
        }

        #[test]
        fn extracts_struct_property() {
            let prop = json!({
                "$type": "StructPropertyData",
                "Value": [
                    {"Name": "X", "$type": "FloatPropertyData", "Value": 1.0},
                    {"Name": "Y", "$type": "FloatPropertyData", "Value": 2.0},
                ],
            });
            assert_eq!(extract_property_value(&prop), json!({"X": 1, "Y": 2}));
        }

        #[test]
        fn extracts_name_property_from_dict() {
            let prop = json!({"$type": "NamePropertyData", "Value": {"Value": "ItemName"}});
            assert_eq!(extract_property_value(&prop), json!("ItemName"));
        }

        #[test]
        fn extracts_soft_object_property() {
            // Due to substring matching, SoftObjectPropertyData is currently
            // processed as ObjectPropertyData (ObjectPropertyData's marker
            // entry is checked first, and it's a substring of
            // SoftObjectPropertyData). The value dict has no "Index" field,
            // so it defaults to 0.
            let prop = json!({
                "$type": "SoftObjectPropertyData",
                "Value": {"AssetPathName": "/Game/Textures/Icon"},
            });
            assert_eq!(extract_property_value(&prop), json!("Reference: 0"));
        }

        #[test]
        fn returns_raw_value_for_unknown_type() {
            let prop = json!({"$type": "UnknownPropertyData", "Value": "some value"});
            assert_eq!(extract_property_value(&prop), json!("some value"));
        }

        #[test]
        fn extracts_name_property_direct_value() {
            let prop = json!({"$type": "NamePropertyData", "Value": "DirectName"});
            assert_eq!(extract_property_value(&prop), json!("DirectName"));
        }

        #[test]
        fn extracts_text_property_from_dict_value() {
            let prop = json!({
                "$type": "TextPropertyData",
                "Value": {"CultureInvariantString": "From Dict"},
            });
            assert_eq!(extract_property_value(&prop), json!("From Dict"));
        }

        #[test]
        fn extracts_text_property_none_value() {
            let prop = json!({"$type": "TextPropertyData", "Value": null});
            assert_eq!(extract_property_value(&prop), json!(""));
        }

        #[test]
        fn extracts_object_property_other_value() {
            let prop = json!({"$type": "ObjectPropertyData", "Value": "some_string"});
            assert_eq!(extract_property_value(&prop), json!("some_string"));
        }

        #[test]
        fn extracts_array_property_non_list() {
            let prop = json!({"$type": "ArrayPropertyData", "Value": "not a list"});
            assert_eq!(extract_property_value(&prop), json!("not a list"));
        }

        #[test]
        fn extracts_set_property() {
            let prop = json!({
                "$type": "SetPropertyData",
                "Value": [
                    {"$type": "IntPropertyData", "Value": 1},
                    {"$type": "IntPropertyData", "Value": 2},
                ],
            });
            assert_eq!(extract_property_value(&prop), json!([1, 2]));
        }

        #[test]
        fn extracts_set_property_non_list() {
            let prop = json!({"$type": "SetPropertyData", "Value": "not a list"});
            assert_eq!(extract_property_value(&prop), json!("not a list"));
        }

        #[test]
        fn extracts_map_property() {
            let prop = json!({"$type": "MapPropertyData", "Value": {"key": "value"}});
            assert_eq!(extract_property_value(&prop), json!({"key": "value"}));
        }

        #[test]
        fn extracts_struct_property_non_list() {
            let prop = json!({"$type": "StructPropertyData", "Value": {"X": 1.0, "Y": 2.0}});
            assert_eq!(extract_property_value(&prop), json!({"X": 1, "Y": 2}));
        }

        #[test]
        fn extracts_delegate_property() {
            let prop = json!({"$type": "DelegatePropertyData", "Value": {"Object": "SomeObject"}});
            assert_eq!(
                extract_property_value(&prop),
                json!({"Object": "SomeObject"})
            );
        }

        #[test]
        fn extracts_fdelegate_property() {
            let prop = json!({"$type": "FDelegate", "Value": "delegate_value"});
            assert_eq!(extract_property_value(&prop), json!("delegate_value"));
        }

        #[test]
        fn extracts_multicast_delegate_list() {
            let prop = json!({
                "$type": "MulticastSparseDelegatePropertyData",
                "Value": [{"func": "A"}, {"func": "B"}],
            });
            assert_eq!(
                extract_property_value(&prop),
                json!([{"func": "A"}, {"func": "B"}])
            );
        }

        #[test]
        fn extracts_multicast_delegate_non_list() {
            // Due to substring matching order, MulticastSparseDelegatePropertyData
            // is matched by DelegatePropertyData first, so returns raw value.
            let prop =
                json!({"$type": "MulticastSparseDelegatePropertyData", "Value": "not a list"});
            assert_eq!(extract_property_value(&prop), json!("not a list"));
        }

        #[test]
        fn extracts_int8_property() {
            let prop = json!({"$type": "Int8PropertyData", "Value": 127});
            assert_eq!(extract_property_value(&prop), json!(127));
        }

        #[test]
        fn extracts_int16_property() {
            let prop = json!({"$type": "Int16PropertyData", "Value": 32000});
            assert_eq!(extract_property_value(&prop), json!(32000));
        }

        #[test]
        fn extracts_int64_property() {
            let prop = json!({"$type": "Int64PropertyData", "Value": 9223372036854775807i64});
            assert_eq!(extract_property_value(&prop), json!(9223372036854775807i64));
        }

        #[test]
        fn extracts_uint16_property() {
            let prop = json!({"$type": "UInt16PropertyData", "Value": 65535});
            assert_eq!(extract_property_value(&prop), json!(65535));
        }

        #[test]
        fn extracts_uint32_property() {
            let prop = json!({"$type": "UInt32PropertyData", "Value": 4294967295u32});
            assert_eq!(extract_property_value(&prop), json!(4294967295u32));
        }

        #[test]
        fn extracts_byte_property() {
            let prop = json!({"$type": "BytePropertyData", "Value": 255});
            assert_eq!(extract_property_value(&prop), json!(255));
        }

        #[test]
        fn extracts_str_property_none_value() {
            let prop = json!({"$type": "StrPropertyData", "Value": null});
            assert_eq!(extract_property_value(&prop), json!(""));
        }

        #[test]
        fn extracts_bool_property_non_bool() {
            let prop = json!({"$type": "BoolPropertyData", "Value": "not a bool"});
            assert_eq!(extract_property_value(&prop), json!(false));
        }

        #[test]
        fn array_with_non_typed_items() {
            let prop = json!({
                "$type": "ArrayPropertyData",
                "Value": [
                    {"$type": "IntPropertyData", "Value": 1},
                    "plain_string",
                    42,
                ],
            });
            assert_eq!(
                extract_property_value(&prop),
                json!([1, "plain_string", 42])
            );
        }
    }
}
