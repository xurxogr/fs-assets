//! Field rule engine for projecting the catalog onto smaller variants.
//!
//! The full catalog carries every field extracted from the game blueprints.
//! A *rule set* projects each item down to a chosen subset of fields by
//! applying an ordered list of include/exclude rules over dotted field
//! paths.
//!
//! [`REQUIRED_PATHS`] lists the fields the two real consumers (the template
//! DB build and the clipboard conversion) read; [`missing_required_paths`]
//! checks a rule set against them so callers can warn when a projection
//! would break those consumers. The `FS` preset is the rule set whose output
//! equals that minimum.
//!
//! Evaluation semantics:
//!
//! * Items are flattened to dotted leaf paths, descending into dicts only
//!   (scalars and lists are leaves at their path).
//! * A rule's pattern *covers* a leaf when it matches the leaf or an
//!   ancestor of it (so `include ItemProfileData` keeps the whole subtree).
//!   Segment globs: `*` matches one segment, `**` matches one or more
//!   segments.
//! * A leaf is kept iff the **last** rule covering it is `include` (the
//!   default, when no rule covers it, is to keep). An empty rule set
//!   therefore keeps everything (the `FULL` preset).

use std::path::Path;

use serde_json::{Map, Value};

/// Whether a rule keeps (include) or drops (exclude) the fields it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    Include,
    Exclude,
}

impl RuleAction {
    fn as_str(&self) -> &'static str {
        match self {
            RuleAction::Include => "include",
            RuleAction::Exclude => "exclude",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "include" => Some(RuleAction::Include),
            "exclude" => Some(RuleAction::Exclude),
            _ => None,
        }
    }
}

/// A single include/exclude rule over a dotted field path pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRule {
    pub action: RuleAction,
    pub pattern: String,
}

/// An ordered list of rules, optionally named, applied to the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CatalogRuleSet {
    pub name: Option<String>,
    pub rules: Vec<CatalogRule>,
}

/// A built-in rule-set preset callers can seed an editor with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogPreset {
    Full,
    Fs,
}

impl CatalogPreset {
    pub fn as_str(&self) -> &'static str {
        match self {
            CatalogPreset::Full => "full",
            CatalogPreset::Fs => "fs",
        }
    }

    const ALL: [CatalogPreset; 2] = [CatalogPreset::Full, CatalogPreset::Fs];

    /// Convert a string to a `CatalogPreset`; never fails, defaulting to
    /// `Full` for invalid/empty input.
    pub fn from_string(value: Option<&str>) -> CatalogPreset {
        let Some(value) = value else {
            return CatalogPreset::Full;
        };
        let normalized = value.trim().to_lowercase();
        if normalized.is_empty() {
            return CatalogPreset::Full;
        }
        for preset in Self::ALL {
            if normalized == preset.as_str() {
                return preset;
            }
        }
        CatalogPreset::Full
    }
}

/// Field paths the two real consumers read. Any rule set that excludes one
/// of these would break the DB build or clipboard conversion.
pub const REQUIRED_PATHS: &[&str] = &[
    "CodeName",
    "FactionVariant",
    "DisplayName",
    "DisplayNameLocales",
    "Icon",
    "SubTypeIcon",
    "StockpileCategory",
    "bIsCratable",
];

/// The FS preset: drop everything, then re-include exactly the required
/// minimum.
const FS_RULES: &[(RuleAction, &str)] = &[
    (RuleAction::Exclude, "**"),
    (RuleAction::Include, "CodeName"),
    (RuleAction::Include, "FactionVariant"),
    (RuleAction::Include, "DisplayName"),
    (RuleAction::Include, "DisplayNameLocales"),
    (RuleAction::Include, "Icon"),
    (RuleAction::Include, "SubTypeIcon"),
    (RuleAction::Include, "StockpileCategory"),
    (RuleAction::Include, "bIsCratable"),
];

/// Return the seed rule set for a preset: empty for `Full` (keep
/// everything), or the required-minimum rule set for `Fs`.
pub fn preset_ruleset(preset: CatalogPreset) -> CatalogRuleSet {
    match preset {
        CatalogPreset::Full => CatalogRuleSet {
            name: Some(CatalogPreset::Full.as_str().to_string()),
            rules: Vec::new(),
        },
        CatalogPreset::Fs => CatalogRuleSet {
            name: Some(CatalogPreset::Fs.as_str().to_string()),
            rules: FS_RULES
                .iter()
                .map(|(action, pattern)| CatalogRule {
                    action: *action,
                    pattern: pattern.to_string(),
                })
                .collect(),
        },
    }
}

/// Return the preset a rule set's rules match, or `None` if it is custom.
///
/// Only the rules are compared (the name is ignored), so an
/// edited-but-equivalent rule set is still recognised as its preset.
pub fn detect_preset(ruleset: &CatalogRuleSet) -> Option<CatalogPreset> {
    CatalogPreset::ALL
        .into_iter()
        .find(|&preset| ruleset.rules == preset_ruleset(preset).rules)
}

/// Match path segments against pattern segments (`*` = 1, `**` = 1+).
fn glob(pattern_segs: &[&str], path_segs: &[&str]) -> bool {
    let Some((head, rest)) = pattern_segs.split_first() else {
        return path_segs.is_empty();
    };

    if *head == "**" {
        if path_segs.is_empty() {
            return false;
        }
        // Consume one segment for "**" and either stop or keep consuming.
        return glob(rest, &path_segs[1..]) || glob(pattern_segs, &path_segs[1..]);
    }

    let Some((path_head, path_rest)) = path_segs.split_first() else {
        return false;
    };
    if *head == "*" || head == path_head {
        return glob(rest, path_rest);
    }
    false
}

/// Whether a pattern covers a leaf path or any of its ancestors.
fn covers(pattern_segs: &[&str], path_segs: &[&str]) -> bool {
    (1..=path_segs.len()).any(|k| glob(pattern_segs, &path_segs[..k]))
}

/// Decide whether a leaf path is kept (last covering rule wins).
fn decide(rules: &[CatalogRule], path_segs: &[&str]) -> bool {
    let mut keep = true;
    for rule in rules {
        let pattern_segs: Vec<&str> = rule.pattern.split('.').collect();
        if covers(&pattern_segs, path_segs) {
            keep = rule.action == RuleAction::Include;
        }
    }
    keep
}

/// Flatten a dict into (path, value) leaves, descending into (non-empty)
/// dicts only.
fn leaf_paths(item: &Map<String, Value>, prefix: &[String]) -> Vec<(Vec<String>, Value)> {
    let mut leaves = Vec::new();
    for (key, value) in item {
        let mut path = prefix.to_vec();
        path.push(key.clone());

        if let Value::Object(m) = value {
            if !m.is_empty() {
                leaves.extend(leaf_paths(m, &path));
                continue;
            }
        }
        leaves.push((path, value.clone()));
    }
    leaves
}

/// Rebuild a nested dict from kept (path, value) leaves.
fn unflatten(leaves: Vec<(Vec<String>, Value)>) -> Map<String, Value> {
    let mut root = Map::new();
    for (path, value) in leaves {
        let mut node = &mut root;
        for segment in &path[..path.len() - 1] {
            node = node
                .entry(segment.clone())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("intermediate path segments are always objects");
        }
        node.insert(path.last().unwrap().clone(), value);
    }
    root
}

fn project_item(item: &Map<String, Value>, rules: &[CatalogRule]) -> Map<String, Value> {
    let leaves = leaf_paths(item, &[]);
    let kept: Vec<(Vec<String>, Value)> = leaves
        .into_iter()
        .filter(|(path, _)| {
            let segs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
            decide(rules, &segs)
        })
        .collect();
    unflatten(kept)
}

/// Project a full catalog through a rule set.
///
/// Returns the catalog unchanged when the rule set is empty (keep
/// everything), or a new list of projected entries otherwise.
pub fn apply_rules(catalog: &[Value], ruleset: &CatalogRuleSet) -> Vec<Value> {
    if ruleset.rules.is_empty() {
        return catalog.to_vec();
    }
    catalog
        .iter()
        .map(|item| match item {
            Value::Object(m) => Value::Object(project_item(m, &ruleset.rules)),
            other => other.clone(),
        })
        .collect()
}

/// Return the required field paths a rule set would exclude (empty when the
/// rule set keeps the full required minimum).
pub fn missing_required_paths(ruleset: &CatalogRuleSet) -> Vec<String> {
    REQUIRED_PATHS
        .iter()
        .filter(|path| {
            let segs: Vec<&str> = path.split('.').collect();
            !decide(&ruleset.rules, &segs)
        })
        .map(|s| s.to_string())
        .collect()
}

fn ruleset_to_value(ruleset: &CatalogRuleSet) -> Value {
    let rules: Vec<Value> = ruleset
        .rules
        .iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert(
                "action".to_string(),
                Value::String(r.action.as_str().to_string()),
            );
            m.insert("pattern".to_string(), Value::String(r.pattern.clone()));
            Value::Object(m)
        })
        .collect();

    let mut root = Map::new();
    root.insert(
        "name".to_string(),
        match &ruleset.name {
            Some(n) => Value::String(n.clone()),
            None => Value::Null,
        },
    );
    root.insert("rules".to_string(), Value::Array(rules));
    Value::Object(root)
}

fn ruleset_from_value(value: &Value) -> anyhow::Result<CatalogRuleSet> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected a JSON object"))?;

    let name = match obj.get("name") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => anyhow::bail!("\"name\" must be a string or null"),
    };

    let rules_value = obj
        .get("rules")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("\"rules\" must be an array"))?;

    let mut rules = Vec::with_capacity(rules_value.len());
    for rule_value in rules_value {
        let rule_obj = rule_value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("each rule must be an object"))?;
        let action_str = rule_obj
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("rule \"action\" must be a string"))?;
        let action = RuleAction::from_str(action_str)
            .ok_or_else(|| anyhow::anyhow!("invalid rule action: {action_str}"))?;
        let pattern = rule_obj
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("rule \"pattern\" must be a string"))?
            .to_string();
        rules.push(CatalogRule { action, pattern });
    }

    Ok(CatalogRuleSet { name, rules })
}

/// Export a rule set to a JSON file.
pub fn save_ruleset(path: &Path, ruleset: &CatalogRuleSet) -> anyhow::Result<()> {
    let value = ruleset_to_value(ruleset);
    let text = serde_json::to_string_pretty(&value)?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Import a rule set from a JSON file.
pub fn load_ruleset(path: &Path) -> anyhow::Result<CatalogRuleSet> {
    let text =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("Invalid rule set file: {e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("Invalid rule set file: {e}"))?;
    ruleset_from_value(&value).map_err(|e| anyhow::anyhow!("Invalid rule set file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ruleset(pairs: &[(&str, &str)]) -> CatalogRuleSet {
        CatalogRuleSet {
            name: None,
            rules: pairs
                .iter()
                .map(|(a, p)| CatalogRule {
                    action: RuleAction::from_str(a).unwrap(),
                    pattern: p.to_string(),
                })
                .collect(),
        }
    }

    fn sample_item() -> Value {
        json!({
            "CodeName": "RifleC",
            "FactionVariant": "EFactionId::Colonials",
            "DisplayName": "Argenti r.II Rifle",
            "DisplayNameLocales": {"en": "Argenti r.II Rifle", "de": "Argenti r.II Gewehr"},
            "Icon": "War/Content/Textures/UI/ItemIcons/RifleIcon.0",
            "ItemProfileData": {"bIsCratable": true, "bIsCriticalToVictory": false},
            "StockpileCategory": "Items",
            "bIsStockpilable": true,
            "bIsCratable": true,
            "Description": "A rifle.",
            "AmmoDynamicData": {"DamageType": {"Type": "Foo"}},
        })
    }

    // ===== Matching / ordering =====

    #[test]
    fn empty_ruleset_keeps_everything() {
        let catalog = vec![sample_item()];
        let result = apply_rules(&catalog, &CatalogRuleSet::default());
        assert_eq!(result, catalog);
    }

    #[test]
    fn last_match_wins() {
        let rules = ruleset(&[("exclude", "**"), ("include", "CodeName")]);
        let result = apply_rules(&[sample_item()], &rules);
        assert_eq!(result, vec![json!({"CodeName": "RifleC"})]);
    }

    #[test]
    fn single_star_matches_one_segment() {
        let rules = ruleset(&[("exclude", "ItemProfileData.*")]);
        let result = apply_rules(&[sample_item()], &rules);
        let item = result[0].as_object().unwrap();
        assert!(!item.contains_key("ItemProfileData"));
        assert_eq!(item["CodeName"], json!("RifleC"));
    }

    #[test]
    fn double_star_matches_subtree_but_not_self() {
        let item = json!({"ShippableInfo": "EShippableType::Large", "Other": {"a": 1, "b": 2}});
        let rules = ruleset(&[("exclude", "Other.**")]);
        let result = apply_rules(&[item], &rules);
        assert_eq!(
            result,
            vec![json!({"ShippableInfo": "EShippableType::Large"})]
        );
    }

    #[test]
    fn prefix_include_keeps_whole_subtree() {
        let rules = ruleset(&[("exclude", "**"), ("include", "DisplayNameLocales")]);
        let result = apply_rules(&[sample_item()], &rules);
        assert_eq!(
            result,
            vec![
                json!({"DisplayNameLocales": {"en": "Argenti r.II Rifle", "de": "Argenti r.II Gewehr"}})
            ]
        );
    }

    #[test]
    fn apply_rules_does_not_mutate_input() {
        let catalog = vec![sample_item()];
        apply_rules(&catalog, &preset_ruleset(CatalogPreset::Fs));
        assert_eq!(catalog, vec![sample_item()]);
    }

    // ===== Presets =====

    #[test]
    fn full_preset_is_empty() {
        assert!(preset_ruleset(CatalogPreset::Full).rules.is_empty());
    }

    #[test]
    fn fs_preset_trims_to_minimum() {
        let result = apply_rules(&[sample_item()], &preset_ruleset(CatalogPreset::Fs));
        let item = result[0].as_object().unwrap();
        assert!(!item.contains_key("Description"));
        assert!(!item.contains_key("AmmoDynamicData"));
        assert!(!item.contains_key("ItemProfileData"));
        assert!(!item.contains_key("bIsStockpilable"));
        assert_eq!(item["StockpileCategory"], json!("Items"));
        assert_eq!(item["bIsCratable"], json!(true));
    }

    #[test]
    fn detect_preset_recognises_presets_and_flags_custom() {
        assert_eq!(
            detect_preset(&preset_ruleset(CatalogPreset::Full)),
            Some(CatalogPreset::Full)
        );
        assert_eq!(
            detect_preset(&preset_ruleset(CatalogPreset::Fs)),
            Some(CatalogPreset::Fs)
        );
        assert_eq!(detect_preset(&ruleset(&[("exclude", "CodeName")])), None);
    }

    #[test]
    fn preset_from_string_never_fails() {
        assert_eq!(CatalogPreset::from_string(None), CatalogPreset::Full);
        assert_eq!(CatalogPreset::from_string(Some("")), CatalogPreset::Full);
        assert_eq!(
            CatalogPreset::from_string(Some("bogus")),
            CatalogPreset::Full
        );
        assert_eq!(CatalogPreset::from_string(Some("fs")), CatalogPreset::Fs);
        assert_eq!(
            CatalogPreset::from_string(Some("  FS  ")),
            CatalogPreset::Fs
        );
        assert_eq!(
            CatalogPreset::from_string(Some("FULL")),
            CatalogPreset::Full
        );
    }

    // ===== Validation =====

    #[test]
    fn presets_satisfy_required_minimum() {
        assert!(missing_required_paths(&preset_ruleset(CatalogPreset::Full)).is_empty());
        assert!(missing_required_paths(&preset_ruleset(CatalogPreset::Fs)).is_empty());
    }

    #[test]
    fn missing_required_reports_dropped_field() {
        let rules: Vec<CatalogRule> = preset_ruleset(CatalogPreset::Fs)
            .rules
            .into_iter()
            .filter(|r| r.pattern != "CodeName")
            .collect();
        let missing = missing_required_paths(&CatalogRuleSet { name: None, rules });
        assert!(missing.contains(&"CodeName".to_string()));
    }

    // ===== Serialization =====

    #[test]
    fn ruleset_round_trips_through_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ruleset = preset_ruleset(CatalogPreset::Fs);
        let path = tmp.path().join("rules.json");
        save_ruleset(&path, &ruleset).unwrap();
        assert_eq!(load_ruleset(&path).unwrap().rules, ruleset.rules);
    }

    #[test]
    fn load_ruleset_rejects_bad_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_ruleset(&path).is_err());
    }
}
