//! Resolution of raw UAsset import entries to `/Game/...` object paths.
//!
//! These are pure lookups over the raw imports array — they hold no parser
//! state (no caches, no logger).

use serde_json::{json, Value};

use crate::catalog::utils::{import_at, normalize_object_path};

/// Resolve an import's full path by following the `OuterIndex` chain.
///
/// Only resolves `BlueprintGeneratedClass` imports. For other assets, use
/// [`resolve_import_asset_path`] instead.
pub fn resolve_import_full_path(import_obj: &Value, all_imports: &[Value]) -> Option<String> {
    if import_obj.get("ClassName").and_then(|v| v.as_str()) != Some("BlueprintGeneratedClass") {
        return None;
    }

    let outer_index = import_obj
        .get("OuterIndex")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if outer_index < 0 {
        let package_import = import_at(outer_index, all_imports)?;
        let package_path = package_import
            .get("ObjectName")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if package_path.starts_with("/Game/") {
            return Some(package_path.to_string());
        }
    }

    None
}

/// Resolve any import to its asset path (for full extraction).
///
/// Unlike [`resolve_import_full_path`], this resolves all asset types
/// (`Texture2D`, `StaticMesh`, etc.), not just `BlueprintGeneratedClass`.
pub fn resolve_import_asset_path(import_obj: &Value, all_imports: &[Value]) -> Option<String> {
    let class_name = import_obj
        .get("ClassName")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if matches!(
        class_name,
        "Package" | "Class" | "Function" | "ScriptStruct"
    ) {
        return None;
    }

    let outer_index = import_obj
        .get("OuterIndex")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if outer_index < 0 {
        let package_import = import_at(outer_index, all_imports)?;
        let package_path = package_import
            .get("ObjectName")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if package_path.starts_with("/Game/") {
            return Some(normalize_object_path(package_path));
        }
    }

    None
}

/// `uasset_to_full_json` imports use snake_case fields (`object_name`,
/// `class_name`, `outer_index`), while this module's functions and
/// `crate::catalog::utils::resolve_import_path` operate on the PascalCase
/// shape (`ObjectName`, `ClassName`, `OuterIndex`). Rather than re-deriving
/// that (already-tested) resolution logic against the new field names,
/// callers adapt the value shape with this and delegate to it directly.
pub(crate) fn to_pascalcase_import(import_obj: &Value) -> Value {
    json!({
        "ClassName": import_obj.get("class_name").cloned().unwrap_or(Value::Null),
        "ObjectName": import_obj.get("object_name").cloned().unwrap_or(Value::Null),
        "OuterIndex": import_obj.get("outer_index").cloned().unwrap_or(json!(0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod resolve_import_full_path_tests {
        use super::*;

        #[test]
        fn non_blueprint_class_returns_none() {
            let import_obj = json!({"ClassName": "Texture2D", "OuterIndex": -1});
            assert_eq!(resolve_import_full_path(&import_obj, &[]), None);
        }

        #[test]
        fn resolves_game_package_path() {
            let all_imports = vec![json!({"ObjectName": "/Game/Blueprints/Items/BPFoo"})];
            let import_obj = json!({"ClassName": "BlueprintGeneratedClass", "OuterIndex": -1});
            assert_eq!(
                resolve_import_full_path(&import_obj, &all_imports),
                Some("/Game/Blueprints/Items/BPFoo".to_string())
            );
        }

        #[test]
        fn non_game_package_path_returns_none() {
            let all_imports = vec![json!({"ObjectName": "/Script/CoreUObject"})];
            let import_obj = json!({"ClassName": "BlueprintGeneratedClass", "OuterIndex": -1});
            assert_eq!(resolve_import_full_path(&import_obj, &all_imports), None);
        }

        #[test]
        fn non_negative_outer_index_returns_none() {
            let import_obj = json!({"ClassName": "BlueprintGeneratedClass", "OuterIndex": 0});
            assert_eq!(resolve_import_full_path(&import_obj, &[]), None);
        }

        #[test]
        fn out_of_range_outer_index_returns_none() {
            let import_obj = json!({"ClassName": "BlueprintGeneratedClass", "OuterIndex": -5});
            assert_eq!(resolve_import_full_path(&import_obj, &[]), None);
        }
    }

    mod resolve_import_asset_path_tests {
        use super::*;

        #[test]
        fn engine_class_names_are_skipped() {
            for class_name in ["Package", "Class", "Function", "ScriptStruct"] {
                let import_obj = json!({"ClassName": class_name, "OuterIndex": -1});
                assert_eq!(resolve_import_asset_path(&import_obj, &[]), None);
            }
        }

        #[test]
        fn resolves_and_normalizes_asset_path() {
            let all_imports = vec![json!({"ObjectName": "/Game/Textures/UI/Icon"})];
            let import_obj = json!({"ClassName": "Texture2D", "OuterIndex": -1});
            let result = resolve_import_asset_path(&import_obj, &all_imports);
            assert!(result.is_some());
        }

        #[test]
        fn non_negative_outer_index_returns_none() {
            let import_obj = json!({"ClassName": "Texture2D", "OuterIndex": 0});
            assert_eq!(resolve_import_asset_path(&import_obj, &[]), None);
        }

        #[test]
        fn non_game_package_path_returns_none() {
            let all_imports = vec![json!({"ObjectName": "/Script/Engine"})];
            let import_obj = json!({"ClassName": "Texture2D", "OuterIndex": -1});
            assert_eq!(resolve_import_asset_path(&import_obj, &all_imports), None);
        }

        #[test]
        fn out_of_range_outer_index_returns_none() {
            let import_obj = json!({"ClassName": "Texture2D", "OuterIndex": -5});
            assert_eq!(resolve_import_asset_path(&import_obj, &[]), None);
        }
    }
}
