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

/// Truthiness check over a `serde_json::Value`: null, `false`, zero, and
/// empty strings/arrays/objects are falsy.
pub(crate) fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
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
        // A signed integer string becomes a number, unless it doesn't fit an
        // `i64`, in which case it is kept as the string it was.
        Value::String(s) => match s.strip_prefix(['+', '-']) {
            Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => s
                .parse::<i64>()
                .map_or_else(|_| value.clone(), |n| json!(n)),
            _ => value.clone(),
        },
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
        fn converts_signed_integer_strings_to_numbers() {
            assert_eq!(simplify_value(&json!("+42")), json!(42));
            assert_eq!(simplify_value(&json!("-7")), json!(-7));
            assert_eq!(
                simplify_value(&json!("-9223372036854775808")),
                json!(i64::MIN)
            );
        }

        #[test]
        fn keeps_signed_integer_strings_that_overflow_i64() {
            for s in ["+99999999999999999999", "-9223372036854775809"] {
                assert_eq!(simplify_value(&json!(s)), json!(s));
            }
        }

        #[test]
        fn keeps_strings_that_are_not_signed_integers() {
            for s in ["+", "-", "42", "+4a", "--1", ""] {
                assert_eq!(simplify_value(&json!(s)), json!(s));
            }
        }

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
}
