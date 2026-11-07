//! The named locations of every hex of the world map: its towns (`Major`) and
//! the lesser names of its areas, with their position on the map.
//!
//! Each hex is a map of its own, `War/Content/Maps/Master/<Hex>.umap`, and
//! each location in it is an export of the class `MapMarker`:
//!
//! - `MajorText`, a text property, holds the name in its culture-invariant
//!   string. Its key is a bare GUID, and no `.locres` translates it.
//! - `MapMarkerType` is `EMapMarkerType::Major` on a town, and absent on a
//!   lesser name.
//! - `RootComponent` points to the export whose `RelativeLocation` vector
//!   places the marker, in world units from the centre of the hex.
//!
//! Positions are given in the map's own coordinates, from 0 to 1 across the
//! hex, the ones a save's `NormalizedMapCoords` and the War API use.
//!
//! The hex is named after its map file, exactly as spelled there: the
//! Deadlands map is `DeadlandsHex`, while saves call it `DeadLandsHex`, so a
//! reader should compare hex names without regard to case.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter};
use unreal_asset::exports::{Export, ExportBaseTrait, ExportNormalTrait};
use unreal_asset::properties::{Property, PropertyDataTrait};
use unreal_asset::types::PackageIndex;
use unreal_asset::Asset;

use crate::catalog::pipeline::CatalogSource;

/// Directory of the pak, and of an extraction, holding one map per hex.
pub const MAPS_PREFIX: &str = "War/Content/Maps/Master/";

/// Extension of a map file.
const MAP_EXTENSION: &str = "umap";

/// Class of the exports that name a location.
const MARKER_CLASS: &str = "MapMarker";

/// `MapMarkerType` of a town.
const MAJOR_TYPE: &str = "EMapMarkerType::Major";

/// Width and height of a hex in world units: a position `X`, `Y` from its
/// centre lies at `0.5 + X / WIDTH`, `0.5 + Y / HEIGHT` on the map.
const HEX_WIDTH: f64 = 218_400.0;
const HEX_HEIGHT: f64 = 189_000.0;

/// Decimals kept of a map coordinate.
const COORD_DECIMALS: i32 = 6;

/// A named location of a hex.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MapLocation {
    /// The name shown on the map, e.g. `Lord's Mouth`.
    pub name: String,
    /// Whether it is a town rather than the lesser name of an area.
    pub major: bool,
    /// Position across the map, from 0 (left) to 1 (right).
    pub x: f64,
    /// Position down the map, from 0 (top) to 1 (bottom).
    pub y: f64,
}

/// Every hex's locations, by hex name (its map's file name), each list
/// sorted by name. The JSON written by [`build_map_locations_to_file`].
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct MapLocations {
    pub hexes: BTreeMap<String, Vec<MapLocation>>,
}

/// What a map-locations build did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MapLocationsStats {
    /// Maps read, one per hex.
    pub maps: usize,
    /// Locations written, towns included.
    pub locations: usize,
    /// Of those, the towns.
    pub major: usize,
    /// Markers left out for want of a name or a position.
    pub skipped: usize,
}

/// A `MapMarker` export as read from a map, before it is checked.
#[derive(Debug, Clone, PartialEq)]
struct Marker {
    /// Name of the export, to tell which one a warning is about.
    export: String,
    name: Option<String>,
    major: bool,
    /// `RelativeLocation` of its root component, in world units.
    position: Option<(f64, f64)>,
}

/// `value` rounded to [`COORD_DECIMALS`] decimals.
fn round_coord(value: f64) -> f64 {
    let scale = 10f64.powi(COORD_DECIMALS);
    (value * scale).round() / scale
}

/// The map coordinates of the world position `x`, `y` (from the centre of
/// the hex), rounded to 6 decimals.
pub fn map_coords(x: f64, y: f64) -> (f64, f64) {
    (
        round_coord(0.5 + x / HEX_WIDTH),
        round_coord(0.5 + y / HEX_HEIGHT),
    )
}

/// Whether `pak_path` (as a pak index records it) is a hex's map: a `.umap`
/// right in [`MAPS_PREFIX`], not in a directory below it.
pub fn is_map_input(pak_path: &str) -> bool {
    pak_path
        .trim_start_matches('/')
        .strip_prefix(MAPS_PREFIX)
        .and_then(|name| name.strip_suffix(".umap"))
        .is_some_and(|stem| !stem.is_empty() && !stem.contains('/'))
}

/// The import `index` refers to in `asset`, if it is one.
fn import_name<C: Read + Seek>(asset: &Asset<C>, index: PackageIndex) -> Option<String> {
    if !index.is_import() {
        return None;
    }
    let import = asset.imports.get(usize::try_from(-index.index - 1).ok()?)?;
    Some(import.object_name.get_content())
}

/// The property named `name` among `properties`.
fn property<'a>(properties: &'a [Property], name: &str) -> Option<&'a Property> {
    properties
        .iter()
        .find(|p| p.get_name().get_content() == name)
}

/// The X and Y of the `RelativeLocation` of `component`.
fn relative_location(component: &Export) -> Option<(f64, f64)> {
    let normal = component.get_normal_export()?;
    let Property::StructProperty(location) = property(&normal.properties, "RelativeLocation")?
    else {
        return None;
    };
    location.value.iter().find_map(|p| match p {
        Property::VectorProperty(v) => Some((v.value.x.0, v.value.y.0)),
        _ => None,
    })
}

/// Every `MapMarker` export of `asset`. Only the properties a marker is read
/// by are looked at: formatting a map's whole data (its landscape above all)
/// takes minutes.
fn markers_in<C: Read + Seek>(asset: &Asset<C>) -> Vec<Marker> {
    let mut markers = Vec::new();
    for export in &asset.asset_data.exports {
        let base = export.get_base_export();
        if import_name(asset, base.class_index).as_deref() != Some(MARKER_CLASS) {
            continue;
        }
        let Some(normal) = export.get_normal_export() else {
            continue;
        };
        let properties = &normal.properties;
        let name = match property(properties, "MajorText") {
            Some(Property::TextProperty(text)) => text.culture_invariant_string.clone(),
            _ => None,
        };
        let major = match property(properties, "MapMarkerType") {
            Some(Property::EnumProperty(kind)) => kind
                .value
                .as_ref()
                .is_some_and(|value| value.get_content() == MAJOR_TYPE),
            _ => false,
        };
        let position = match property(properties, "RootComponent") {
            Some(Property::ObjectProperty(root)) => asset
                .asset_data
                .get_export(root.value)
                .and_then(relative_location),
            _ => None,
        };
        markers.push(Marker {
            export: base.object_name.get_content(),
            name,
            major,
            position,
        });
    }
    markers
}

/// The `MapMarker` exports of the map at `path`.
fn read_markers(path: &Path) -> anyhow::Result<Vec<Marker>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let asset = crate::convert::parse_asset(BufReader::new(file), path)?;
    Ok(markers_in(&asset))
}

/// The locations of `hex` out of its `markers`, sorted by name, leaving out,
/// with a warning, a marker without a name or a usable position. Returns the
/// locations and how many markers were left out.
fn locations_of(hex: &str, markers: Vec<Marker>) -> (Vec<MapLocation>, usize) {
    let mut locations = Vec::with_capacity(markers.len());
    let mut skipped = 0;
    for marker in markers {
        let Some(name) = marker.name.filter(|name| !name.trim().is_empty()) else {
            log::warn!("map-locations: {hex}: marker {} has no name", marker.export);
            skipped += 1;
            continue;
        };
        let Some((x, y)) = marker
            .position
            .filter(|(x, y)| x.is_finite() && y.is_finite())
        else {
            log::warn!("map-locations: {hex}: marker {name} has no position");
            skipped += 1;
            continue;
        };
        let (x, y) = map_coords(x, y);
        locations.push(MapLocation {
            name,
            major: marker.major,
            x,
            y,
        });
    }
    locations.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(b.major.cmp(&a.major))
            .then(a.x.total_cmp(&b.x))
            .then(a.y.total_cmp(&b.y))
    });
    (locations, skipped)
}

/// Every map file in the maps directory of `extract_dir`, sorted, failing on
/// a directory that is missing or reached through a symlink.
fn collect_maps(extract_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !extract_dir.is_dir() {
        anyhow::bail!("extraction directory not found: {}", extract_dir.display());
    }
    let maps_dir = extract_dir.join(MAPS_PREFIX.trim_end_matches('/'));
    if !maps_dir.is_dir() {
        anyhow::bail!("maps directory not found: {}", maps_dir.display());
    }
    if crate::pak::has_symlink_below(extract_dir, &maps_dir) {
        anyhow::bail!("{} is reached through a symlink", maps_dir.display());
    }
    let mut maps = Vec::new();
    for entry in
        std::fs::read_dir(&maps_dir).with_context(|| format!("reading {}", maps_dir.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", maps_dir.display()))?;
        // Not followed: a link in there could point anywhere.
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some(MAP_EXTENSION) {
            maps.push(path);
        }
    }
    maps.sort();
    Ok(maps)
}

/// Reads the locations of every hex from an extraction directory, reporting
/// `(done, total)` over the maps to `progress`. Fails when there is no map,
/// when one can't be read, and when a hex has no town: each hex's locations
/// are named after its towns.
pub fn build_map_locations_from_extract_dir(
    extract_dir: &Path,
    progress: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<(MapLocations, MapLocationsStats)> {
    let maps = collect_maps(extract_dir)?;
    if maps.is_empty() {
        anyhow::bail!(
            "no maps in {}",
            extract_dir
                .join(MAPS_PREFIX.trim_end_matches('/'))
                .display()
        );
    }

    let mut stats = MapLocationsStats {
        maps: maps.len(),
        ..Default::default()
    };
    let mut result = MapLocations::default();
    progress(0, maps.len())?;
    for (index, map) in maps.iter().enumerate() {
        let hex = map
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("{} has no usable name", map.display()))?
            .to_string();
        let markers = read_markers(map).with_context(|| format!("reading the map of {hex}"))?;
        let (locations, skipped) = locations_of(&hex, markers);
        stats.skipped += skipped;
        stats.locations += locations.len();
        stats.major += locations.iter().filter(|l| l.major).count();
        result.hexes.insert(hex, locations);
        progress(index + 1, maps.len())?;
    }

    let without_town: Vec<&str> = result
        .hexes
        .iter()
        .filter(|(_, locations)| !locations.iter().any(|l| l.major))
        .map(|(hex, _)| hex.as_str())
        .collect();
    if !without_town.is_empty() {
        anyhow::bail!("no Major location in {}", without_town.join(", "));
    }
    Ok((result, stats))
}

/// Extracts `pak`'s maps ([`is_map_input`]) into a temporary directory,
/// removed when the returned handle drops, reporting `(done, total)` over the
/// extracted files to `extracting`.
fn extract_maps(
    pak: &Path,
    extracting: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<tempfile::TempDir> {
    let workspace = tempfile::tempdir()?;
    // The game's ~53 maps are ~170 MB each, ~9 GB in all.
    let mut budget = crate::pak::ExtractBudget {
        max_entry_bytes: 1 << 30,
        remaining_bytes: 32 << 30,
    };
    let extracted = crate::pak::extract_matching_within(
        pak,
        is_map_input,
        workspace.path(),
        extracting,
        &mut budget,
    )?;
    if extracted.is_empty() {
        anyhow::bail!(
            "{} has no maps (no .umap under {MAPS_PREFIX})",
            pak.display()
        );
    }
    Ok(workspace)
}

/// Reads the locations of every hex from `source` and writes them to
/// `output` as JSON (see [`write_map_locations`]). Progress is reported over
/// a pak's extracted maps to `extracting` and over the maps read to
/// `building`. Nothing is written when the build fails.
pub fn build_map_locations_to_file(
    source: CatalogSource<'_>,
    output: &Path,
    extracting: &dyn Fn(usize, usize) -> anyhow::Result<()>,
    building: &dyn Fn(usize, usize) -> anyhow::Result<()>,
) -> anyhow::Result<MapLocationsStats> {
    let (locations, stats) = match source {
        CatalogSource::ExtractDir(dir) => build_map_locations_from_extract_dir(dir, building)?,
        CatalogSource::Pak(pak) => {
            let workspace = extract_maps(pak, extracting)?;
            build_map_locations_from_extract_dir(workspace.path(), building)?
        }
    };
    write_map_locations(output, &locations)?;
    Ok(stats)
}

/// Pretty-printed JSON with every number that has a fraction written with
/// [`COORD_DECIMALS`] decimals, so a coordinate always reads `0.829300`.
struct FixedDecimals<'a>(PrettyFormatter<'a>);

impl Formatter for FixedDecimals<'_> {
    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        write!(writer, "{value:.prec$}", prec = COORD_DECIMALS as usize)
    }

    fn begin_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_value(writer)
    }
}

/// Replaces `output` with `locations` as `{"hexes": {"<Hex>": [{"name",
/// "major", "x", "y"}, ...]}}`, hexes and their locations sorted and
/// coordinates written with 6 decimals, so a rebuild from
/// the same game gives the same file.
pub fn write_map_locations(output: &Path, locations: &MapLocations) -> anyhow::Result<()> {
    let mut temp = crate::atomic_write::temp_next_to(output, ".json.tmp")?;
    let mut writer = BufWriter::new(temp.as_file_mut());
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut writer, FixedDecimals(PrettyFormatter::new()));
    locations.serialize(&mut serializer)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    drop(writer);
    crate::atomic_write::persist(temp.into_temp_path(), output)
        .with_context(|| format!("writing {}", output.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    fn marker(name: Option<&str>, major: bool, position: Option<(f64, f64)>) -> Marker {
        Marker {
            export: "MapMarker_1".to_string(),
            name: name.map(str::to_string),
            major,
            position,
        }
    }

    #[test]
    fn world_positions_become_map_coordinates() {
        // Swordfort, in Acrithia, as the War API places it.
        let (x, y) = map_coords(-50055.15625, -39050.71875);
        assert_eq!(
            ((x * 1e4).round() / 1e4, (y * 1e4).round() / 1e4),
            (0.2708, 0.2934)
        );
        assert_eq!(map_coords(0.0, 0.0), (0.5, 0.5));
        assert_eq!(map_coords(HEX_WIDTH / 2.0, -HEX_HEIGHT / 2.0), (1.0, 0.0));
    }

    #[test]
    fn coordinates_keep_six_decimals() {
        let (x, _) = map_coords(28232.875, -59222.78125);
        assert_eq!(x, 0.629271);
    }

    #[test]
    fn map_inputs_are_the_umaps_right_in_the_master_directory() {
        assert!(is_map_input("War/Content/Maps/Master/WestgateHex.umap"));
        assert!(is_map_input("/War/Content/Maps/Master/MarbanHollow.umap"));
        assert!(!is_map_input("War/Content/Maps/Master/WestgateHex.uexp"));
        assert!(!is_map_input(
            "War/Content/Maps/Master/Sub/WestgateHex.umap"
        ));
        assert!(!is_map_input("War/Content/Maps/Master/.umap"));
        assert!(!is_map_input("War/Content/Maps/Other/WestgateHex.umap"));
    }

    #[test]
    fn keeps_named_placed_markers_sorted_by_name() {
        let (locations, skipped) = locations_of(
            "WestgateHex",
            vec![
                marker(Some("Westgate Keep"), true, Some((0.0, 0.0))),
                marker(Some("Lord's Mouth"), true, Some((71_478.12, -13_191.3))),
                marker(Some("The Gallows"), false, Some((-10.0, 10.0))),
            ],
        );

        assert_eq!(skipped, 0);
        let names: Vec<&str> = locations.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Lord's Mouth", "The Gallows", "Westgate Keep"]);
        assert_eq!(
            locations[2],
            MapLocation {
                name: "Westgate Keep".to_string(),
                major: true,
                x: 0.5,
                y: 0.5,
            }
        );
        assert!(!locations[1].major);
    }

    #[test]
    fn leaves_out_a_marker_without_a_name_or_a_position() {
        let (locations, skipped) = locations_of(
            "AcrithiaHex",
            vec![
                marker(None, true, Some((0.0, 0.0))),
                marker(Some("  "), false, Some((0.0, 0.0))),
                marker(Some("Swordfort"), true, None),
                marker(Some("Nowhere"), false, Some((f64::NAN, 0.0))),
                marker(
                    Some("Astero's Spear"),
                    false,
                    Some((28232.875, -59222.78125)),
                ),
            ],
        );

        assert_eq!(skipped, 4);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].name, "Astero's Spear");
    }

    #[test]
    fn writes_sorted_hexes_with_six_decimal_coordinates() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("map_locations.json");
        let mut locations = MapLocations::default();
        for hex in ["WestgateHex", "AcrithiaHex"] {
            locations.hexes.insert(
                hex.to_string(),
                vec![MapLocation {
                    name: "Lord's Mouth".to_string(),
                    major: true,
                    x: 0.8293,
                    y: 0.5,
                }],
            );
        }

        write_map_locations(&output, &locations).unwrap();

        let text = fs::read_to_string(&output).unwrap();
        assert!(text.contains("\"x\": 0.829300,"), "{text}");
        assert!(text.contains("\"y\": 0.500000\n"), "{text}");
        assert!(text.find("AcrithiaHex").unwrap() < text.find("WestgateHex").unwrap());
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed["hexes"]["WestgateHex"][0],
            serde_json::json!({"name": "Lord's Mouth", "major": true, "x": 0.8293, "y": 0.5})
        );
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn fails_without_a_maps_directory_or_without_maps() {
        let tmp = tempfile::tempdir().unwrap();
        let none = |_, _| Ok(());

        let error = build_map_locations_from_extract_dir(&tmp.path().join("nope"), &none)
            .unwrap_err()
            .to_string();
        assert!(error.contains("extraction directory not found"), "{error}");

        let error = build_map_locations_from_extract_dir(tmp.path(), &none)
            .unwrap_err()
            .to_string();
        assert!(error.contains("maps directory not found"), "{error}");

        let maps = tmp.path().join(MAPS_PREFIX);
        fs::create_dir_all(&maps).unwrap();
        fs::write(maps.join("notes.txt"), b"").unwrap();
        let error = build_map_locations_from_extract_dir(tmp.path(), &none)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("no maps in"), "{error}");
    }

    #[test]
    fn a_map_that_fails_to_parse_fails_the_build_naming_its_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let maps = tmp.path().join(MAPS_PREFIX);
        fs::create_dir_all(&maps).unwrap();
        fs::write(maps.join("BrokenHex.umap"), b"not a map").unwrap();
        let output = tmp.path().join("map_locations.json");

        let error = build_map_locations_to_file(
            CatalogSource::ExtractDir(tmp.path()),
            &output,
            &|_, _| Ok(()),
            &|_, _| Ok(()),
        )
        .unwrap_err();

        assert!(
            crate::cli::error_message(&error).contains("reading the map of BrokenHex"),
            "{error:#}"
        );
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_maps_directory_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(outside.join("Master")).unwrap();
        let extract = tmp.path().join("extract");
        fs::create_dir_all(extract.join("War/Content")).unwrap();
        std::os::unix::fs::symlink(&outside, extract.join("War/Content/Maps")).unwrap();

        let error = build_map_locations_from_extract_dir(&extract, &|_, _| Ok(()))
            .unwrap_err()
            .to_string();

        assert!(error.contains("through a symlink"), "{error}");
    }

    /// Packs `files` (pak paths, with `contents`) into a pak.
    fn write_pak(dir: &Path, files: &[&str], contents: &[u8]) -> PathBuf {
        let content = dir.join("content");
        for file in files {
            let path = content.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let pak_path = dir.join("test.pak");
        crate::pak::pack(&content, &pak_path, "../../../", repak::Version::V8B).unwrap();
        pak_path
    }

    #[test]
    fn a_pak_without_maps_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let pak = write_pak(tmp.path(), &["War/Content/Maps/Other/A.umap"], b"");

        let error = build_map_locations_to_file(
            CatalogSource::Pak(&pak),
            &tmp.path().join("out.json"),
            &|_, _| Ok(()),
            &|_, _| Ok(()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("has no maps"), "{error}");
    }

    #[test]
    fn extracts_only_the_maps_of_a_pak() {
        let tmp = tempfile::tempdir().unwrap();
        let pak = write_pak(
            tmp.path(),
            &[
                "War/Content/Maps/Master/AcrithiaHex.umap",
                "War/Content/Maps/Master/MarbanHollow.umap",
                "War/Content/Maps/Master/Sub/Other.umap",
                "War/Content/Blueprints/Data/placeholder.txt",
            ],
            b"not a map",
        );

        let extracting = RefCell::new(Vec::new());
        let building = RefCell::new(Vec::new());
        let error = build_map_locations_to_file(
            CatalogSource::Pak(&pak),
            &tmp.path().join("out.json"),
            &|done, total| {
                extracting.borrow_mut().push((done, total));
                Ok(())
            },
            &|done, total| {
                building.borrow_mut().push((done, total));
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(extracting.into_inner(), vec![(0, 2), (1, 2), (2, 2)]);
        assert_eq!(building.into_inner(), vec![(0, 2)]);
        assert!(
            crate::cli::error_message(&error).contains("reading the map of AcrithiaHex"),
            "{error:#}"
        );
    }

    /// The real maps of two hexes, copied from the game's pak into
    /// `tests/fixtures/maps` (not distributable).
    fn fixture_maps() -> tempfile::TempDir {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/maps");
        let tmp = tempfile::tempdir().unwrap();
        let maps = tmp.path().join(MAPS_PREFIX);
        fs::create_dir_all(&maps).unwrap();
        for hex in ["AcrithiaHex", "WestgateHex"] {
            let name = format!("{hex}.umap");
            fs::copy(fixtures.join(&name), maps.join(&name)).unwrap();
        }
        tmp
    }

    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn reads_the_towns_and_areas_of_real_maps() {
        let tmp = fixture_maps();

        let (locations, stats) =
            build_map_locations_from_extract_dir(tmp.path(), &|_, _| Ok(())).unwrap();

        assert_eq!(stats.maps, 2);
        assert_eq!(stats.skipped, 0);
        let acrithia = &locations.hexes["AcrithiaHex"];
        assert_eq!(acrithia.len(), 14);
        assert_eq!(acrithia.iter().filter(|l| l.major).count(), 8);
        let swordfort = acrithia.iter().find(|l| l.name == "Swordfort").unwrap();
        assert!(swordfort.major);
        assert_eq!(
            (swordfort.x, swordfort.y),
            map_coords(-50055.15625, -39050.71875)
        );
        let spear = acrithia
            .iter()
            .find(|l| l.name == "Astero's Spear")
            .unwrap();
        assert!(!spear.major);
        assert_eq!((spear.x, spear.y), map_coords(28232.875, -59222.78125));

        let westgate = &locations.hexes["WestgateHex"];
        assert_eq!(westgate.len(), 37);
        let towns: Vec<&str> = westgate
            .iter()
            .filter(|l| l.major)
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(
            towns,
            [
                "Holdfast",
                "Kingstone",
                "Longstone",
                "Lord's Mouth",
                "Lost Partition",
                "Rancher's Fast",
                "The Gallows",
                "Westgate Keep",
                "Wyattwick",
                "Zeus' Demise",
            ]
        );
        assert_eq!(stats.locations, 14 + 37);
        assert_eq!(stats.major, 8 + 10);
    }
}
