//! Catalog builder logic.
//!
//! `blueprint_parser`, `catalog_assembler`, `damage_type_resolver` and
//! `data_table_lookup` are pure, self-contained transformations over
//! `serde_json::Value` (mirroring the raw whole-asset-dump JSON format)
//! plus loader closures; `pipeline` is the filesystem-backed integration
//! layer that wires them together into the `build-catalog` CLI command (see
//! `crate::cli`).

pub mod blueprint_parser;
pub mod catalog_assembler;
pub mod catalog_field_filter;
pub mod damage_type_resolver;
pub mod data_table_lookup;
pub mod import_resolution;
pub mod localization;
pub mod pipeline;
pub mod stockpile_classifier;
pub mod type_processing;
pub mod utils;
