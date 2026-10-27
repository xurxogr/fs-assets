//! Progress reporting for an `add-mod` or `extract-icons` run: which stage
//! it is in and how far through that stage it is, so a caller (the CLI, or a
//! GUI through the Python bindings) can show it.

use std::fmt;

/// One stage of an `add-mod` or `extract-icons` run, in the order a run goes
/// through them.
/// Stages that don't apply (e.g. extraction when the source is already a
/// directory) are skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Extracting the catalog's icons from the mod's `.pak`.
    ExtractingIcons,
    /// Extracting the catalog's icons from the vanilla `.pak` fallback.
    ExtractingVanillaIcons,
    /// Decoding icons into templates, one step per catalog entry.
    BuildingTemplates,
    /// Writing `extract-icons`' PNGs, one step per catalog item.
    WritingIcons,
}

impl Stage {
    /// Stable snake_case name, as passed to Python progress callbacks.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::ExtractingIcons => "extracting_icons",
            Stage::ExtractingVanillaIcons => "extracting_vanilla_icons",
            Stage::BuildingTemplates => "building_templates",
            Stage::WritingIcons => "writing_icons",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One progress report: `done` of `total` steps of `stage` are finished.
/// Every stage reports `done == 0` when it starts and `done == total` when
/// it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: usize,
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_are_snake_case() {
        assert_eq!(Stage::BuildingTemplates.to_string(), "building_templates");
        assert_eq!(
            Stage::ExtractingVanillaIcons.as_str(),
            "extracting_vanilla_icons"
        );
    }
}
