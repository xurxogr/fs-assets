//! Foxhole game asset tools: read the game's `.pak` directly, build the item
//! catalog and the icon-template database used for stockpile OCR, and work
//! with `.locres` translations and mod paks. The same operations are exposed
//! by the `fs-assets` CLI ([`cli`]) and, with the `python` feature, by the
//! `fs_assets` Python package.

pub mod add_mod;
mod atomic_write;
pub mod catalog;
pub mod cli;
pub mod convert;
pub mod database;
pub mod grep;
pub mod locres;
pub mod pak;

// Python bindings (when compiled with --features python). Kept as one
// narrow, intentional surface over the modules above — everything else
// stays a plain Rust API.
#[cfg(feature = "python")]
mod python;
