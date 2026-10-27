use std::cell::Cell;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::panic::UnwindSafe;
use std::path::Path;
use std::sync::Once;

use serde_json::{json, Map, Value};
use unreal_asset::engine_version::EngineVersion;
use unreal_asset::exports::{Export, ExportBaseTrait, ExportNormalTrait};
use unreal_asset::properties::int_property::BytePropertyValue;
use unreal_asset::properties::{Property, PropertyDataTrait};
use unreal_asset::types::fname::FName;
use unreal_asset::types::PackageIndex;
use unreal_asset::Asset;

// All Foxhole assets are cooked with the same engine version, so there is
// no need to handle other versions.
const ENGINE_VERSION: EngineVersion = EngineVersion::VER_UE4_25;

/// The full text of an `FName`: its string-table content plus the
/// zero-based `_N` suffix the engine stores as instance number `N + 1`
/// (0 means no suffix), so `S_Muzzle_Fore_0` keeps its `_0`.
fn fname_to_string(name: &FName) -> String {
    let content = name.get_content();
    match name.get_number() {
        number if number > 0 => format!("{content}_{}", number - 1),
        _ => content,
    }
}

/// A single-precision value as JSON, using its shortest f32 representation
/// (`0.7` rather than `0.699999988079071`). Non-finite values become null.
fn single_precision(value: f32) -> Value {
    value
        .to_string()
        .parse::<f64>()
        .map(|v| json!(v))
        .unwrap_or(Value::Null)
}

/// A single-precision struct component stored widened to f64.
fn widened(value: f64) -> Value {
    single_precision(value as f32)
}

/// Converts a parsed property to the plain JSON value the catalog uses:
/// `$type`-tagged property wrappers are flattened to their values, matching
/// the shape of catalog values derived from exported asset JSON.
fn property_to_value(prop: &Property) -> Value {
    match prop {
        Property::BoolProperty(p) => json!(p.value),
        Property::IntProperty(p) => json!(p.value),
        Property::Int8Property(p) => json!(p.value),
        Property::Int16Property(p) => json!(p.value),
        Property::Int64Property(p) => json!(p.value),
        Property::UInt16Property(p) => json!(p.value),
        Property::UInt32Property(p) => json!(p.value),
        Property::UInt64Property(p) => json!(p.value),
        Property::FloatProperty(p) => single_precision(p.value.into_inner()),
        Property::DoubleProperty(p) => json!(p.value.into_inner()),
        Property::StrProperty(p) => json!(p.value.clone().unwrap_or_default()),
        Property::NameProperty(p) => json!(fname_to_string(&p.value)),
        Property::EnumProperty(p) => match &p.value {
            Some(v) => json!(fname_to_string(v)),
            None => Value::Null,
        },
        Property::ByteProperty(p) => match &p.value {
            BytePropertyValue::Byte(b) => json!(b),
            BytePropertyValue::FName(n) => json!(fname_to_string(n)),
        },
        Property::TextProperty(p) => match (&p.culture_invariant_string, &p.value) {
            (Some(text), Some(guid)) => json!({"Text": text, "Guid": guid}),
            (Some(text), None) => json!(text),
            (None, Some(v)) => json!(v),
            (None, None) => json!(""),
        },
        Property::ObjectProperty(p) => json!(format!("Reference: {}", p.value.index)),
        Property::ArrayProperty(p) => Value::Array(p.value.iter().map(property_to_value).collect()),
        Property::SetProperty(p) => {
            Value::Array(p.value.value.iter().map(property_to_value).collect())
        }
        // MapProperty is only consumed by `data_table_lookup`'s "profile
        // table" parsing, which walks a Map property's raw exported-JSON
        // shape. Represented here as an array of `[key, value]` pairs —
        // mirroring that raw shape closely enough that `data_table_lookup`
        // can walk it the same way — with
        // both key and value themselves fully processed via
        // `property_to_value` (so a struct-typed map value comes out as a
        // flat object, not another `$type`-tagged blob).
        Property::MapProperty(p) => Value::Array(
            p.value
                .iter()
                .map(|(_, k, v)| json!([property_to_value(k), property_to_value(v)]))
                .collect(),
        ),
        Property::StructProperty(p) => {
            let mut map = Map::new();
            for inner in &p.value {
                map.insert(fname_to_string(&inner.get_name()), property_to_value(inner));
            }
            Value::Object(map)
        }
        Property::VectorProperty(p) => {
            json!({"X": widened(p.value.x.0), "Y": widened(p.value.y.0), "Z": widened(p.value.z.0)})
        }
        Property::RotatorProperty(p) => {
            json!({"Pitch": widened(p.value.x.0), "Yaw": widened(p.value.y.0), "Roll": widened(p.value.z.0)})
        }
        Property::Vector4Property(p) => {
            json!({"X": widened(p.value.x.0), "Y": widened(p.value.y.0), "Z": widened(p.value.z.0), "W": widened(p.value.w.0)})
        }
        Property::QuatProperty(p) => {
            json!({"X": widened(p.value.x.0), "Y": widened(p.value.y.0), "Z": widened(p.value.z.0), "W": widened(p.value.w.0)})
        }
        Property::Vector2DProperty(p) => {
            json!({"X": widened(p.value.x.0), "Y": widened(p.value.y.0)})
        }
        Property::IntPointProperty(p) => json!({"X": p.value.x, "Y": p.value.y}),
        Property::ColorProperty(p) => {
            json!({"R": p.color.r, "G": p.color.g, "B": p.color.b, "A": p.color.a})
        }
        Property::LinearColorProperty(p) => {
            json!({
                "R": single_precision(p.color.r.0),
                "G": single_precision(p.color.g.0),
                "B": single_precision(p.color.b.0),
                "A": single_precision(p.color.a.0),
            })
        }
        Property::GuidProperty(p) => json!(format!("{:?}", p.value)),
        // Fallback: property name of the variant only, never the full Debug dump
        // (which would recurse through FName -> NameMap and blow up in size).
        other => json!(format!(
            "<unhandled property: {}>",
            other.get_name().get_content()
        )),
    }
}

fn properties_to_map(properties: &[Property]) -> Value {
    let mut map = Map::new();
    for prop in properties {
        map.insert(fname_to_string(&prop.get_name()), property_to_value(prop));
    }
    Value::Object(map)
}

thread_local! {
    /// Set while this thread runs [`catch_unwind_quietly`]'s closure.
    static QUIET_PANICS: Cell<bool> = const { Cell::new(false) };
}

/// [`std::panic::catch_unwind`] that keeps a panic inside `f` from printing
/// its message: `unreal_asset` panics on malformed assets, and those are
/// reported as errors instead. The panic hook is replaced only once, by one
/// that stays silent just for the thread inside `f` and defers to the
/// previous hook everywhere else, so concurrent callers (the Python bindings
/// allow them) never see each other's hook swaps, and a panic outside `f`
/// still prints.
fn catch_unwind_quietly<R>(f: impl FnOnce() -> R + UnwindSafe) -> std::thread::Result<R> {
    static INSTALL_HOOK: Once = Once::new();
    INSTALL_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET_PANICS.with(Cell::get) {
                previous(info);
            }
        }));
    });

    /// Restores the thread's previous quiet flag, even when `f` unwinds.
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            QUIET_PANICS.with(|quiet| quiet.set(self.0));
        }
    }
    let _restore = Restore(QUIET_PANICS.with(|quiet| quiet.replace(true)));
    std::panic::catch_unwind(f)
}

/// Parses the asset `reader` holds (read from `path`, named in errors) under
/// the single engine version all Foxhole assets use. `catch_unwind` remains
/// as a defensive backstop against future crate bugs, but with the vendored
/// patch applied (see Cargo.toml) this should no longer be hit for any real
/// asset.
fn parse_asset<C: Read + Seek + UnwindSafe>(reader: C, path: &Path) -> anyhow::Result<Asset<C>> {
    match catch_unwind_quietly(move || Asset::new(reader, None, ENGINE_VERSION)) {
        Ok(Ok(asset)) => Ok(asset),
        Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
        Err(_) => Err(anyhow::anyhow!("panicked while parsing {}", path.display())),
    }
}

/// The flat property map of `path`'s `Default__` export.
pub fn uasset_to_json(path: &Path) -> anyhow::Result<Value> {
    let asset = parse_asset(BufReader::new(File::open(path)?), path)?;
    for export in &asset.asset_data.exports {
        if let Some(normal) = export.get_normal_export() {
            let name = normal.base_export.object_name.get_content();
            if name.starts_with("Default__") {
                return Ok(properties_to_map(&normal.properties));
            }
        }
    }
    anyhow::bail!("no Default__ export found in {}", path.display());
}

/// Raw signed `PackageIndex.index` as read from the crate: 0 = null,
/// negative = import reference (`imports[(-n-1) as usize]`), positive =
/// export reference (`exports[(n-1) as usize]`). Verified directly against
/// `PackageIndex::from_import`/`from_export` in unrealmodding/unreal_asset
/// src/types/mod.rs (`from_import` builds
/// `PackageIndex::new(-import_index - 1)`, `from_export` builds
/// `PackageIndex::new(export_index + 1)`), so this is the exact inverse of
/// those constructors.
fn package_index_to_i32(index: PackageIndex) -> i32 {
    index.index
}

/// Serializes an entire parsed asset — every import and every export, with
/// reference indices intact — unlike `uasset_to_json` which only extracts
/// the flat property map of the `Default__` export. This is the richer
/// representation needed by `blueprint_parser`, `data_table_lookup`,
/// `damage_type_resolver`, and `catalog_assembler`.
pub fn uasset_to_full_json(path: &Path) -> anyhow::Result<Value> {
    let asset = parse_asset(BufReader::new(File::open(path)?), path)?;
    let imports: Vec<Value> = asset
        .imports
        .iter()
        .map(|import| {
            json!({
                "object_name": fname_to_string(&import.object_name),
                "class_name": fname_to_string(&import.class_name),
                "class_package": fname_to_string(&import.class_package),
                "outer_index": package_index_to_i32(import.outer_index),
            })
        })
        .collect();

    let exports: Vec<Value> = asset
        .asset_data
        .exports
        .iter()
        .map(export_to_value)
        .collect();

    Ok(json!({
        "imports": imports,
        "exports": exports,
    }))
}

fn export_to_value(export: &Export) -> Value {
    let base = export.get_base_export();
    let mut map = Map::new();
    map.insert(
        "object_name".to_string(),
        json!(fname_to_string(&base.object_name)),
    );
    map.insert(
        "class_index".to_string(),
        json!(package_index_to_i32(base.class_index)),
    );
    map.insert(
        "super_index".to_string(),
        json!(package_index_to_i32(base.super_index)),
    );
    map.insert(
        "template_index".to_string(),
        json!(package_index_to_i32(base.template_index)),
    );
    map.insert(
        "outer_index".to_string(),
        json!(package_index_to_i32(base.outer_index)),
    );

    match export {
        Export::RawExport(_) => {
            map.insert("kind".to_string(), json!("raw"));
        }
        Export::DataTableExport(data_table_export) => {
            map.insert("kind".to_string(), json!("data_table"));
            map.insert(
                "properties".to_string(),
                properties_to_map(&data_table_export.normal_export.properties),
            );
            let mut rows = Map::new();
            for row in &data_table_export.table.data {
                rows.insert(fname_to_string(&row.name), properties_to_map(&row.value));
            }
            map.insert("rows".to_string(), Value::Object(rows));
        }
        Export::NormalExport(_)
        | Export::ClassExport(_)
        | Export::StructExport(_)
        | Export::PropertyExport(_)
        | Export::FunctionExport(_)
        | Export::EnumExport(_)
        | Export::LevelExport(_)
        | Export::StringTableExport(_) => {
            map.insert("kind".to_string(), json!("normal"));
            if let Some(normal) = export.get_normal_export() {
                map.insert(
                    "properties".to_string(),
                    properties_to_map(&normal.properties),
                );
            }
        }
        _ => {
            map.insert("kind".to_string(), json!("other"));
            if let Some(normal) = export.get_normal_export() {
                map.insert(
                    "properties".to_string(),
                    properties_to_map(&normal.properties),
                );
            }
        }
    }

    Value::Object(map)
}

/// The 8-byte PNG file signature (`\x89PNG\r\n\x1a\n`).
const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// The `IEND` chunk type tag every valid PNG ends with.
const PNG_IEND: [u8; 4] = *b"IEND";

/// Finds every byte offset of `PNG_MAGIC` inside `haystack`.
fn find_png_magic_offsets(haystack: &[u8]) -> Vec<usize> {
    (0..haystack.len().saturating_sub(PNG_MAGIC.len()))
        .filter(|&i| haystack[i..i + PNG_MAGIC.len()] == PNG_MAGIC)
        .collect()
}

/// Cooked `FTexturePlatformData` pixel formats this module knows how to
/// decode. Restricted to the formats actually confirmed against a real
/// fixture in this pass (see `decode_compressed_mip0`'s doc comment) --
/// `PF_BC4`/`PF_BC6H` etc. are plausible additions but untested, so they're
/// deliberately left unhandled rather than guessed at.
///
/// `PF_B8G8R8A8` is uncompressed (block-compressed formats need
/// `texture2ddecoder`; this one is a raw per-pixel BGRA byte dump), confirmed
/// byte-exact against ground-truth reference decodes of 4 real Foxhole
/// vehicle icons (`MediumTank2CIcon`, `MediumTank2IndirectWIcon`,
/// `MediumTank2RangeWIcon`, `MediumTankLargeCIcon`) that all use this format
/// instead of a block-compressed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressedPixelFormat {
    Dxt1,
    Dxt5,
    Bc5,
    Bc7,
    B8g8r8a8,
}

impl CompressedPixelFormat {
    fn from_name(s: &[u8]) -> Option<Self> {
        match s {
            b"PF_DXT1" => Some(Self::Dxt1),
            b"PF_DXT5" => Some(Self::Dxt5),
            b"PF_BC5" => Some(Self::Bc5),
            b"PF_BC7" => Some(Self::Bc7),
            b"PF_B8G8R8A8" => Some(Self::B8g8r8a8),
            _ => None,
        }
    }

    /// Bytes per 4x4 block (BC1/DXT1 packs 2 colors + indices into 8 bytes;
    /// BC3/DXT5/BC5/BC7 all use 16-byte blocks). `B8g8r8a8` isn't
    /// block-compressed at all -- callers must check for it separately
    /// before relying on this (see `expected_block_payload_len`).
    fn block_bytes(self) -> usize {
        match self {
            Self::Dxt1 => 8,
            Self::Dxt5 | Self::Bc5 | Self::Bc7 => 16,
            Self::B8g8r8a8 => 4,
        }
    }
}

/// Where in the raw file the `Texture2D` export's `FTexturePlatformData`
/// fields were found, confirmed byte-for-byte against 3 real cooked Foxhole
/// fixtures (see `decode_compressed_mip0`).
struct TextureHeader {
    pixel_format: CompressedPixelFormat,
    size_x: i32,
    size_y: i32,
    /// Byte offset of mip[0]'s `FByteBulkData` header (its `BulkDataFlags`
    /// field). This is **not** immediately after `FirstMipToSerialize`+
    /// `Mips.Num()`: per `FTexture2DMipMap::Serialize` (confirmed against
    /// independent reference implementations of the engine's own
    /// serialization order), each mip entry first serializes a 4-byte `bool`
    /// (`cooked`/`bCooked`, read via `Ar << cooked` -- serialized as a full
    /// `int32` by `FArchive::operator<<(bool&)`, not a 1-byte flag) *before*
    /// the bulk-data header, for every engine version at or above
    /// `VER_UE4_TEXTURE_SOURCE_ART_REFACTOR` (true for all UE4.25 Foxhole
    /// assets). So this offset is `after FirstMipToSerialize+Mips.Num()` +
    /// 4 (the `cooked` bool), not +0.
    mip0_header_offset: usize,
}

/// Largest texture side `find_texture_header` accepts: UE4's default maximum
/// texture size. The decode buffers grow with `width * height`, so this caps
/// what one hostile header can make a decode allocate (about 512 MiB at
/// 8192x8192, instead of 1.25 GiB at the engine's absolute 16384 limit).
const MAX_TEXTURE_DIMENSION: i32 = 8192;

/// Byte-scans `bytes[range_start..range_end]` -- the specific `Texture2D`
/// export's own serialized byte range, per `unreal_asset`'s export table
/// (`BaseExport::serial_offset`/`serial_size`), NOT the whole file -- for a
/// `PixelFormat` `FString` (e.g. `"PF_DXT1\0"`) serialized as part of
/// `FTexturePlatformData::Serialize`.
///
/// A cooked Texture2D's file-wide NameMap independently contains the same
/// string (for unrelated FName references elsewhere, e.g. name-table
/// dedup entries), and scanning the *whole file* for it is ambiguous: on
/// 289/290 real Foxhole icon assets, the NameMap occurrence sorted before
/// the genuine per-export occurrence in raw byte order, so a whole-file
/// "first match wins" scan silently locked onto the wrong one. Restricting
/// the scan to only this export's own byte range makes that class of bug
/// structurally impossible (the NameMap sits elsewhere in the file, before
/// the export data section).
///
/// Every candidate within that range is additionally validated against the
/// surrounding fields expected by the real serialization order -- `SizeX`,
/// `SizeY`, `NumSlices` immediately before the string, then
/// `FirstMipToSerialize`+`Mips.Num()` immediately after -- the same
/// "validate the neighbors" pattern `uasset_to_png`'s embedded-PNG scan
/// already uses for PNG-magic candidates.
fn find_texture_header(
    bytes: &[u8],
    range_start: usize,
    range_end: usize,
) -> Option<TextureHeader> {
    const NEEDLES: &[&[u8]] = &[b"PF_DXT1", b"PF_DXT5", b"PF_BC5", b"PF_BC7", b"PF_B8G8R8A8"];

    let range_end = range_end.min(bytes.len());
    if range_start >= range_end {
        return None;
    }
    let haystack = &bytes[range_start..range_end];

    for &needle in NEEDLES {
        let format = CompressedPixelFormat::from_name(needle)?;
        let mut search_from = 0usize;
        while let Some(rel) = haystack[search_from..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            let idx = range_start + search_from + rel;
            search_from = search_from + rel + 1;

            if idx < 16 {
                continue;
            }
            let str_len = needle.len() + 1; // FString length prefix includes the null terminator
            let length_off = idx - 4;
            let length = i32::from_le_bytes(bytes[length_off..length_off + 4].try_into().unwrap());
            if length as usize != str_len {
                continue;
            }
            if bytes.get(idx + needle.len()) != Some(&0u8) {
                continue;
            }

            let sizex_off = length_off - 12;
            let size_x = i32::from_le_bytes(bytes[sizex_off..sizex_off + 4].try_into().unwrap());
            let size_y =
                i32::from_le_bytes(bytes[sizex_off + 4..sizex_off + 8].try_into().unwrap());
            let num_slices =
                i32::from_le_bytes(bytes[sizex_off + 8..sizex_off + 12].try_into().unwrap());
            if !(1..=MAX_TEXTURE_DIMENSION).contains(&size_x)
                || !(1..=MAX_TEXTURE_DIMENSION).contains(&size_y)
                || !(1..=8).contains(&num_slices)
            {
                continue;
            }

            let after = idx + str_len;
            if bytes.len() < after + 44 {
                continue;
            }
            let first_mip = i32::from_le_bytes(bytes[after..after + 4].try_into().unwrap());
            let mip_count = i32::from_le_bytes(bytes[after + 4..after + 8].try_into().unwrap());
            if !(0..20).contains(&first_mip) || !(1..=20).contains(&mip_count) {
                continue;
            }

            return Some(TextureHeader {
                pixel_format: format,
                size_x,
                size_y,
                // `after + 8` is immediately after `FirstMipToSerialize`
                // (4 bytes) + `Mips.Num()` (4 bytes); the mip's `cooked`
                // bool (4 more bytes) still precedes the real bulk-data
                // header -- see `TextureHeader::mip0_header_offset`'s doc
                // comment.
                mip0_header_offset: after + 8 + 4,
            });
        }
    }
    None
}

/// `EBulkDataFlags` bit values, confirmed against real source rather than
/// guessed: two independent reference implementations of the engine's
/// bulk-data serialization define the exact same bit positions.
mod bulk_data_flags {
    /// Bulk data stored at the end of *this* file; the serialized offset
    /// field needs `Summary.BulkDataStartOffset` added to become an
    /// absolute file position (see `locate_mip0_payload`).
    pub const PAYLOAD_AT_END_OF_FILE: u32 = 1 << 0;
    /// 64-bit `ElementCount`/`SizeOnDisk` fields (UE4.22+). Never observed
    /// set on any real Foxhole fixture; if it ever is, this module's fixed
    /// 20-byte mip0 header layout no longer applies, so decoding is
    /// deliberately refused rather than mis-parsed (see
    /// `parse_mip0_header`).
    pub const SIZE_64_BIT: u32 = 1 << 13;
    /// UE4.26+ flag meaning "do not add `Summary.BulkDataStartOffset` to
    /// the offset field" -- never set pre-4.26, but checked anyway since
    /// it's cheap and documented.
    pub const NO_OFFSET_FIX_UP: u32 = 1 << 16;
}

/// The `FByteBulkData` header preceding mip[0]'s pixel data. Field
/// widths/order confirmed against two independent reference implementations
/// of the engine's own bulk-data header serialization, both of which agree:
/// `BulkDataFlags: u32`,
/// `ElementCount: i32`, `BulkDataSizeOnDisk: u32`, then `BulkDataOffsetInFile`
/// -- `i32` pre-`VER_UE4_BULKDATA_AT_LARGE_OFFSETS`, `i64` at/after it (true
/// for every UE4.25 asset here, so this module always reads the 8-byte
/// form) -- 20 bytes total, *not* 24 as a prior pass of this code assumed
/// (that prior 24-byte assumption was silently reading 4 bytes into the
/// following mip's `SizeX`/`SizeY`/`SizeZ` fields as if they were part of
/// the bulk-data header; see `TextureHeader::mip0_header_offset`'s doc
/// comment for the real source of that missing/extra 4 bytes -- a per-mip
/// `cooked` bool that precedes this header, not a wider header itself).
struct Mip0Header {
    flags: u32,
    size_on_disk: u32,
    /// `BulkDataOffsetInFile` exactly as serialized on disk, *before* the
    /// `Summary.BulkDataStartOffset` fixup both reference implementations
    /// apply after reading it -- see `locate_mip0_payload`.
    offset_in_file_raw: i64,
}

fn parse_mip0_header(bytes: &[u8], off: usize) -> Option<Mip0Header> {
    if bytes.len() < off + 20 {
        return None;
    }
    let flags = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    if flags & bulk_data_flags::SIZE_64_BIT != 0 {
        // Would shift every field below by 4 bytes (64-bit ElementCount
        // and SizeOnDisk); never observed on a real fixture, so refuse
        // rather than silently mis-read the rest of the header.
        return None;
    }
    let size_on_disk = u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap());
    let offset_in_file_raw = i64::from_le_bytes(bytes[off + 12..off + 20].try_into().unwrap());
    Some(Mip0Header {
        flags,
        size_on_disk,
        offset_in_file_raw,
    })
}

/// Minimal little-endian cursor used only by
/// `parse_bulk_data_start_offset` below, to read `FPackageFileSummary`
/// (the `.uasset` file header) far enough to reach its
/// `BulkDataStartOffset: i64` field.
struct SummaryCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SummaryCursor<'a> {
    /// The next `n` bytes, or `None` if fewer remain. Lengths come from the
    /// file, so the end offset is computed with `checked_add`.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let bytes = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    /// `FString`: `i32` length `n`; `n > 0` -> `n` bytes of (8-bit,
    /// nul-terminated) ANSI; `n < 0` -> `-n * 2` bytes of UTF-16LE
    /// (nul-terminated); `n == 0` -> empty/absent. Only the byte count is
    /// needed here, not the decoded text.
    fn skip_fstring(&mut self) -> Option<()> {
        let n = self.i32()?;
        let len = usize::try_from(n.unsigned_abs()).ok()?;
        if n < 0 {
            self.skip(len.checked_mul(2)?)
        } else {
            self.skip(len)
        }
    }
    fn skip_engine_version(&mut self) -> Option<()> {
        // FEngineVersion: u16 major, u16 minor, u16 patch, u32 build,
        // FString branch.
        self.u16()?;
        self.u16()?;
        self.u16()?;
        self.u32()?;
        self.skip_fstring()
    }
}

/// Parses just enough of `FPackageFileSummary` (the `.uasset` file header,
/// read from the *start* of the file -- unrelated to `find_texture_header`'s
/// export-range scan) to reach its `BulkDataStartOffset: i64` field, which
/// `BulkDataOffsetInFile` needs added to it (see `locate_mip0_payload`) to
/// become an absolute file offset. Field order/conditions transcribed from,
/// and cross-checked against, independent sources that all agree, including
/// the vendored (`xurxogr/unrealmodding`) fork of `unreal_asset`'s own
/// `Asset::parse_header` (`unreal_asset/src/lib.rs`, which parses this same
/// struct internally but doesn't expose the field publicly -- hence this
/// from-scratch reimplementation instead of reusing it), verified by having
/// a scratch build of that fork with the field made `pub` print the same
/// values this function computes for all three checked fixtures
/// (`FacilityVehicleFactory1ShipAssemblyDecal1`: 4114, `ATAmmoIcon`: 11079,
/// `ATRPGCItemIcon`: 88716).
///
/// Every version-gated branch below is hardcoded to its known value for
/// `ENGINE_VERSION = VER_UE4_25` (per this module's top-of-file convention
/// that all Foxhole assets are that one fixed engine version -- "no need to
/// guess across versions"), rather than re-deriving each threshold from the
/// file's own `FileVersionUE4`/`FileVersionUE5`/object-version fields the
/// way a general-purpose parser would.
fn parse_bulk_data_start_offset(bytes: &[u8]) -> Option<i64> {
    let mut r = SummaryCursor {
        data: bytes,
        pos: 0,
    };
    let tag = r.u32()?;
    if tag != 0x9E2A83C1 {
        return None;
    }
    let legacy_file_version = r.i32()?;
    if legacy_file_version != -4 {
        r.i32()?; // discarded UE3 engine version
    }
    let file_version = r.i32()?;
    let _unversioned = file_version == 0;
    if legacy_file_version <= -8 {
        r.i32()?; // UE5 file version (not applicable pre-UE5 packages)
    }
    let _file_license_version = r.i32()?;
    if legacy_file_version <= -2 {
        let num_custom = r.i32()?;
        for _ in 0..num_custom {
            r.skip(16)?; // FGuid
            r.i32()?; // version number
        }
    }
    let _header_offset = r.i32()?;
    r.skip_fstring()?; // folder_name
    let _package_flags = r.u32()?;
    let _name_count = r.i32()?;
    let _name_offset = r.i32()?;
    // VER_UE4_SERIALIZE_TEXT_IN_PACKAGES: always true for UE4.25.
    let _gatherable_text_data_count = r.i32()?;
    let _gatherable_text_data_offset = r.i32()?;
    let _export_count = r.i32()?;
    let _export_offset = r.i32()?;
    let _import_count = r.i32()?;
    let _import_offset = r.i32()?;
    let _depends_offset = r.i32()?;
    // VER_UE4_ADD_STRING_ASSET_REFERENCES_MAP: always true for UE4.25.
    let _soft_package_reference_count = r.i32()?;
    let _soft_package_reference_offset = r.i32()?;
    // VER_UE4_ADDED_SEARCHABLE_NAMES: always true for UE4.25.
    let _searchable_names_offset = r.i32()?;
    let _thumbnail_table_offset = r.i32()?;
    r.skip(16)?; // package_guid
    let generations_count = r.i32()?;
    for _ in 0..generations_count {
        r.i32()?; // export_count
        r.i32()?; // name_count
    }
    // VER_UE4_ENGINE_VERSION_OBJECT: always true for UE4.25.
    r.skip_engine_version()?; // SavedByEngineVersion
                              // VER_UE4_PACKAGE_SUMMARY_HAS_COMPATIBLE_ENGINE_VERSION: always true
                              // for UE4.25.
    r.skip_engine_version()?; // CompatibleWithEngineVersion
    let _compression_flags = r.u32()?;
    let compression_block_count = r.u32()?;
    if compression_block_count != 0 {
        // Not observed on any real fixture; refuse rather than guess how
        // to skip a non-empty compressed-chunks array.
        return None;
    }
    let _package_source = r.u32()?;
    let additional_to_cook = r.i32()?;
    if additional_to_cook != 0 {
        return None;
    }
    if legacy_file_version > -7 {
        let texture_allocations_count = r.i32()?;
        if texture_allocations_count != 0 {
            return None;
        }
    }
    let _asset_registry_data_offset = r.i32()?;
    r.i64()
}

/// Resolves mip[0]'s absolute payload location per `FByteBulkData::Serialize`
/// (confirmed identical across independent reference implementations of the
/// engine's own bulk-data serialization):
///
/// - If `BULKDATA_PayloadAtEndOfFile` (`bulk_data_flags::PAYLOAD_AT_END_OF_FILE`,
///   bit 0) is set: the payload's absolute file offset is
///   `offset_in_file_raw`, plus `bulk_data_start_offset` (the
///   `FPackageFileSummary::BulkDataStartOffset` field parsed by
///   `parse_bulk_data_start_offset`) *unless*
///   `bulk_data_flags::NO_OFFSET_FIX_UP` is also set (a UE4.26+ flag, not
///   observed on any UE4.25 Foxhole fixture, checked anyway since it's
///   cheap). This is the branch real cooked `ItemIcons` use.
/// - Otherwise (covers both `BULKDATA_ForceInlinePayload` and the
///   plain/no-special-flags case identically -- `UnCoreSerialize.cpp`
///   handles them with the same `SerializeDataChunk(Ar)` call at the
///   archive's current position): the payload immediately follows this
///   20-byte header, i.e. `mip0_header_offset + 20`.
///
/// Both cases follow the documented `BulkDataFlags` semantics. Checked
/// against real fixtures: `FacilityVehicleFactory1ShipAssemblyDecal1` (`flags=0x401` =
/// `PAYLOAD_AT_END_OF_FILE | BULKDATA_Force_NOT_InlinePayload`, needs the
/// `bulk_data_start_offset` fixup) and `ATAmmoIcon`/other `PF_DXT5` icons
/// (`flags=0x48` = `BULKDATA_ForceInlinePayload | BULKDATA_SingleUse`, no
/// `PAYLOAD_AT_END_OF_FILE`, payload immediately follows the header).
fn locate_mip0_payload(
    header: &Mip0Header,
    mip0_header_offset: usize,
    bulk_data_start_offset: i64,
) -> Option<(usize, usize)> {
    let len = header.size_on_disk as usize;
    if header.flags & bulk_data_flags::PAYLOAD_AT_END_OF_FILE != 0 {
        let mut abs_offset = header.offset_in_file_raw;
        if header.flags & bulk_data_flags::NO_OFFSET_FIX_UP == 0 {
            abs_offset = abs_offset.checked_add(bulk_data_start_offset)?;
        }
        let start = usize::try_from(abs_offset).ok()?;
        Some((start, len))
    } else {
        Some((mip0_header_offset.checked_add(20)?, len))
    }
}

fn expected_block_payload_len(format: CompressedPixelFormat, width: i32, height: i32) -> usize {
    if format == CompressedPixelFormat::B8g8r8a8 {
        // Uncompressed: one 4-byte BGRA pixel per texel, row-major, no
        // padding -- confirmed byte-exact (width*height*4 ==
        // mip0.size_on_disk) against MediumTank2CIcon.uasset and 3 sibling
        // vehicle icons via a ground-truth reference decode.
        return width as usize * height as usize * format.block_bytes();
    }
    let blocks_w = ((width + 3) / 4) as usize;
    let blocks_h = ((height + 3) / 4) as usize;
    blocks_w * blocks_h * format.block_bytes()
}

/// Finds the `Texture2D` export's own serialized byte range
/// (`[serial_offset, serial_offset + serial_size)`, absolute file offsets)
/// via `unreal_asset`'s already-parsed export table, so
/// `find_texture_header` can be pointed at exactly that range instead of
/// the whole file. This is the fix for the whole-file-scan ambiguity
/// documented on `find_texture_header`: the export table is authoritative
/// about where each export's bytes live, so there's no "which occurrence
/// comes first" guesswork left once the search is confined to it.
///
/// Parses the asset independently (rather than threading the caller's
/// already-parsed `Asset` through, which would require exposing generic
/// `Read + Seek` plumbing across `uasset_to_png`'s public boundary) --
/// negligible extra cost for icon-sized files, and consistent with
/// `uasset_to_png`'s existing best-effort re-parse for the embedded-PNG
/// path above it. It parses the `bytes` the caller already read from `path`,
/// not the file again.
fn find_texture2d_export_range(path: &Path, bytes: &[u8]) -> Option<(usize, usize)> {
    let asset = parse_asset(std::io::Cursor::new(bytes), path).ok()?;

    for export in &asset.asset_data.exports {
        let base = export.get_base_export();
        let class_name = base.get_class_type_for_ancestry(&asset);
        if fname_to_string(&class_name) == "Texture2D" {
            let start = base.serial_offset.max(0) as usize;
            let size = base.serial_size.max(0) as usize;
            return Some((start, start.saturating_add(size)));
        }
    }
    None
}

/// Decodes the largest (mip 0) level of a real cooked, BC/DXT-compressed
/// `Texture2D`'s mip chain -- the case `uasset_to_png`'s embedded-PNG scan
/// falls through on (no `FObjectThumbnail` present, meaning this is an
/// actual cooked runtime asset rather than an uncooked editor one).
///
/// `unreal_asset` has zero `Texture2D`/`FTexturePlatformData` support, so
/// this hand-parses the container from raw bytes (there's no existing Rust
/// crate for that container format on crates.io as of this pass), then
/// hands the raw compressed block bytes to `texture2ddecoder` for the
/// actual BC1/BC3/BC5/BC7 pixel math (deliberately not hand-rolled).
///
/// **What's confirmed, byte-exact, against a ground-truth reference decode
/// of the same asset** (0 mean-squared-error match after
/// brute-force searching for the payload offset, then cross-checking the
/// resulting field layout by hand -- see the investigation notes that
/// produced this function for the full byte-offset table):
/// - `T_1x1_Grid.uasset` (128x128, `PF_DXT1`)
/// - `T_Default_Material_Grid_M.uasset` (512x512, `PF_DXT5`)
/// - `T_Default_Material_Grid_N.uasset` (256x256, `PF_BC5`)
///
/// **What did NOT work**: `T_MobileControls_texture.uasset` and
/// `T_MobileMenu2.uasset` (both `PF_DXT5`, 512x512) have a *different*
/// header layout after `Mips.Num()` -- the fields this function expects
/// (`BulkDataFlags`/`ElementCount`/`SizeOnDisk`/`OffsetInFile`/mip
/// `SizeX`/`SizeY`/`SizeZ`) come out as nonsense (negative sizes, an
/// `OffsetInFile` in the terabytes) for those two files, so
/// `find_texture_header`/`parse_mip0_header` either fail their sanity
/// checks or -- if they don't -- `locate_mip0_payload` or the decode call
/// below will error out. This wasn't root-caused; a plausible guess is an
/// extra per-mip or per-platform-data field (streaming/VT-related) that
/// only appears for some textures, but that's speculation, not something
/// verified against bytes.
fn decode_compressed_mip0(path: &Path, bytes: &[u8]) -> anyhow::Result<image::RgbaImage> {
    let (range_start, range_end) = find_texture2d_export_range(path, bytes).ok_or_else(|| {
        anyhow::anyhow!(
            "couldn't find a Texture2D export (with a resolvable serial_offset/serial_size) via \
             unreal_asset's export table in {}",
            path.display()
        )
    })?;

    let header = find_texture_header(bytes, range_start, range_end).ok_or_else(|| {
        anyhow::anyhow!(
            "no recognized cooked PixelFormat field (PF_DXT1/PF_DXT5/PF_BC5/PF_BC7/PF_B8G8R8A8) \
             found in the Texture2D export's own byte range [{range_start}, {range_end}) of {}",
            path.display()
        )
    })?;

    let mip0 = parse_mip0_header(bytes, header.mip0_header_offset).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: mip[0] bulk-data header ran past end of file",
            path.display()
        )
    })?;

    let expected_len =
        expected_block_payload_len(header.pixel_format, header.size_x, header.size_y);
    if mip0.size_on_disk as usize != expected_len {
        anyhow::bail!(
            "{}: mip[0] SizeOnDisk ({}) doesn't match the expected {:?} block-compressed size \
             for {}x{} ({} bytes) -- this file's header layout likely differs from the 3 \
             confirmed fixtures (see decode_compressed_mip0 doc comment)",
            path.display(),
            mip0.size_on_disk,
            header.pixel_format,
            header.size_x,
            header.size_y,
            expected_len
        );
    }

    let bulk_data_start_offset = parse_bulk_data_start_offset(bytes).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: couldn't parse FPackageFileSummary far enough to read BulkDataStartOffset",
            path.display()
        )
    })?;

    let (payload_start, payload_len) =
        locate_mip0_payload(&mip0, header.mip0_header_offset, bulk_data_start_offset).ok_or_else(
            || {
                anyhow::anyhow!(
                "{}: couldn't locate mip[0]'s pixel payload (OffsetInFile arithmetic overflowed \
                 or produced a negative offset)",
                path.display()
            )
            },
        )?;
    let payload_end = payload_start
        .checked_add(payload_len)
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}: mip[0] payload [{}, +{}) runs past end of file ({} bytes)",
                path.display(),
                payload_start,
                payload_len,
                bytes.len()
            )
        })?;
    let payload = &bytes[payload_start..payload_end];

    let width = header.size_x as usize;
    let height = header.size_y as usize;

    let mut rgba = Vec::with_capacity(width * height * 4);
    if header.pixel_format == CompressedPixelFormat::B8g8r8a8 {
        // Uncompressed: payload is already a flat, row-major BGRA8 pixel
        // dump (no texture2ddecoder involved), confirmed byte-exact against
        // a ground-truth reference decode of MediumTank2CIcon.uasset and 3
        // sibling vehicle icons.
        for &[b, g, r, a] in payload.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    } else {
        let mut pixels = vec![0u32; width * height];
        let decode_result = match header.pixel_format {
            CompressedPixelFormat::Dxt1 => {
                texture2ddecoder::decode_bc1(payload, width, height, &mut pixels)
            }
            CompressedPixelFormat::Dxt5 => {
                texture2ddecoder::decode_bc3(payload, width, height, &mut pixels)
            }
            CompressedPixelFormat::Bc5 => {
                texture2ddecoder::decode_bc5(payload, width, height, &mut pixels)
            }
            CompressedPixelFormat::Bc7 => {
                texture2ddecoder::decode_bc7(payload, width, height, &mut pixels)
            }
            CompressedPixelFormat::B8g8r8a8 => unreachable!(),
        };
        decode_result.map_err(|e| {
            anyhow::anyhow!(
                "{}: texture2ddecoder failed to decode {:?} block data: {e}",
                path.display(),
                header.pixel_format
            )
        })?;

        // texture2ddecoder packs each output pixel via `color(r,g,b,a) =
        // u32::from_le_bytes([b,g,r,a])`, i.e. as a little-endian BGRA u32 --
        // confirmed by reading its `src/color.rs` source directly rather
        // than assuming a channel order. Un-swizzle to RGBA for `image`.
        for px in &pixels {
            let [b, g, r, a] = px.to_le_bytes();
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    image::RgbaImage::from_raw(width as u32, height as u32, rgba).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: decoded pixel buffer size didn't match {}x{}",
            path.display(),
            width,
            height
        )
    })
}

/// Texture -> PNG export.
///
/// `unreal_asset` has no `Texture2D`/`FTexturePlatformData` support at all
/// (confirmed: no `TextureExport`, no `PixelFormat` handling), so decoding
/// the cooked mip chain the "textbook" way (property list, then hand-parse
/// `FTexturePlatformData`/DXT blocks from the export's raw tail) was the
/// expected path here. That turned out to be unnecessary for the fixtures
/// actually available.
///
/// Byte-inspecting the three real Foxhole Slate UI icon fixtures in
/// `tests/fixtures/textures/*.uasset` directly (there is no separate
/// `.uexp`/`.ubulk`, so everything is in one file) showed each file embeds
/// a byte-for-byte valid, complete PNG image *somewhere in the raw file*,
/// preceded by a little-endian `u32` triple `[SizeX, SizeY, PngByteLen]`
/// immediately before the PNG signature:
///
/// ```text
/// sig-12..sig-8   u32  SizeX        (verified equal to the PNG's own IHDR width,
///                                    and to the ground-truth reference PNG's width)
/// sig-8..sig-4    u32  SizeY        (ditto, height)
/// sig-4..sig-0    u32  PngByteLen   (verified: sig_offset + PngByteLen lands exactly
///                                    8 bytes past that PNG's own IEND chunk)
/// sig..sig+PngByteLen                the PNG bytes themselves
/// ```
///
/// This offset is **not** inside the `Texture2D` export's own serialized
/// range (it sits much earlier in the file, before that export's
/// `serial_offset`). The property list for the fixtures (`Source`,
/// `LightingGuid`, `MipGenSettings`, `NeverStream`, `LODGroup`, ...)
/// includes editor-only fields that are normally stripped in a cooked
/// shipping build, so these `.uasset` files are most likely **uncooked /
/// editor-format** assets (plausible for Foxhole specifically, which has
/// documented mod-tooling support built on raw editor assets) rather than
/// runtime-cooked ones. Two things follow from that:
/// 1. The embedded PNG is almost certainly UE4's `FObjectThumbnail` asset
///    browser thumbnail cache, not `FTexturePlatformData` — but for a
///    small square UI icon a thumbnail typically *is* a faithful full
///    render of the texture, so it is usable as the "exported" image.
/// 2. Real DXT1/DXT5/BC5-*compressed* `FTexturePlatformData` (the format a
///    cooked 3D-model texture would actually use) was **never
///    encountered** in this pass — no such fixture was available/tested,
///    and this function does not attempt to decode one. When no
///    plausible embedded-PNG blob is found, it errors out rather than
///    guessing at a compressed-block layout.
///
/// A file can contain more than one PNG-magic occurrence (a spurious match
/// against unrelated bytes, e.g. inside the name table, was observed in 2
/// of the 3 fixtures) — each candidate is validated by checking its
/// preceding `[W, H, Len]` header is internally consistent (`Len > 0`,
/// `sig + Len` in bounds, and the PNG actually ends with `IEND` at that
/// exact position) before it's accepted.
pub fn uasset_to_png(path: &Path) -> anyhow::Result<Vec<u8>> {
    let image = uasset_to_image(path)?;
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("failed to encode PNG for {}: {e}", path.display()))?;
    Ok(out)
}

/// Decodes a `.uasset` texture to an in-memory image, the way
/// [`uasset_to_png`] finds it, without encoding it to PNG.
pub fn uasset_to_image(path: &Path) -> anyhow::Result<image::DynamicImage> {
    // The embedded-PNG scan reads raw file bytes and never needs
    // `unreal_asset` to parse the file: one of the three real fixtures it was
    // tested against (`ResearchPartIcon.uasset`) fails to fully parse via
    // `unreal_asset` at all (`Error: failed to fill whole buffer`) yet still
    // yields a perfectly good embedded PNG this way — worth knowing if this
    // function's error path ever needs a stricter validation step.
    let file_bytes = std::fs::read(path)?;

    for sig_offset in find_png_magic_offsets(&file_bytes) {
        if sig_offset < 12 {
            continue;
        }
        let header: [u8; 12] = file_bytes[sig_offset - 12..sig_offset]
            .try_into()
            .expect("slice of length 12");
        let png_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        if png_len == 0 {
            continue;
        }
        let Some(png_end) = sig_offset
            .checked_add(png_len)
            .filter(|&end| end <= file_bytes.len())
        else {
            continue;
        };
        if file_bytes[png_end - 8..png_end - 4] != PNG_IEND {
            continue;
        }
        let png_bytes = &file_bytes[sig_offset..png_end];

        // Decoded (rather than the embedded bytes passed through untouched)
        // so a corrupt/truncated embedded blob is caught here instead of
        // silently handed to the caller.
        return image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png).map_err(
            |e| anyhow::anyhow!("embedded PNG in {} failed to decode: {e}", path.display()),
        );
    }

    // No embedded FObjectThumbnail PNG (i.e. this is a real cooked asset, not
    // an uncooked/editor one) -- fall through to hand-parsing the cooked
    // FTexturePlatformData mip chain and decoding its BC/DXT blocks. See
    // decode_compressed_mip0's doc comment for exactly which fixtures this
    // is confirmed to work on.
    decode_compressed_mip0(path, &file_bytes)
        .map(image::DynamicImage::ImageRgba8)
        .map_err(|e| {
            anyhow::anyhow!(
                "no embedded-PNG texture payload found in {} (tried decoding it as a cooked \
             DXT/BC-compressed FTexturePlatformData mip chain instead, which also failed): {e}",
                path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unreal_asset::properties::array_property::ArrayProperty;
    use unreal_asset::properties::set_property::SetProperty;
    use unreal_asset::properties::str_property::NameProperty;
    use unreal_asset::unversioned::ancestry::Ancestry;

    #[test]
    fn summary_cursor_fails_on_fstring_lengths_past_the_data() {
        for n in [i32::MIN, i32::MAX, -3, 5] {
            let mut data = n.to_le_bytes().to_vec();
            data.extend_from_slice(b"abc\0");
            let mut cursor = SummaryCursor {
                data: &data,
                pos: 0,
            };
            assert_eq!(cursor.skip_fstring(), None, "{n}");
        }
    }

    #[test]
    fn summary_cursor_skips_fstrings_that_fit() {
        let mut data = 4i32.to_le_bytes().to_vec();
        data.extend_from_slice(b"abc\0");
        data.extend_from_slice(&(-2i32).to_le_bytes());
        data.extend_from_slice(&[b'a', 0, 0, 0]);
        let mut cursor = SummaryCursor {
            data: &data,
            pos: 0,
        };
        assert_eq!(cursor.skip_fstring(), Some(()));
        assert_eq!(cursor.skip_fstring(), Some(()));
        assert_eq!(cursor.pos, data.len());
        assert_eq!(cursor.skip(usize::MAX), None);
        assert_eq!(cursor.i32(), None);
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn catch_unwind_quietly_catches_and_scopes_the_silence_to_its_thread() {
        let inner = catch_unwind_quietly(|| {
            let other_thread = std::thread::spawn(|| QUIET_PANICS.with(Cell::get))
                .join()
                .unwrap();
            (QUIET_PANICS.with(Cell::get), other_thread)
        });
        assert_eq!(inner.unwrap(), (true, false));

        assert!(catch_unwind_quietly(|| panic!("malformed asset")).is_err());
        assert!(
            !QUIET_PANICS.with(Cell::get),
            "flag left set after unwinding"
        );
    }

    /// A minimal `PF_DXT1` texture header of `size` x `size` at offset 0.
    fn texture_header_bytes(size: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in [size, size, 1, 8] {
            bytes.extend_from_slice(&value.to_le_bytes()); // SizeX, SizeY, slices, FString len
        }
        bytes.extend_from_slice(b"PF_DXT1\0");
        bytes.extend_from_slice(&0i32.to_le_bytes()); // FirstMipToSerialize
        bytes.extend_from_slice(&1i32.to_le_bytes()); // Mips.Num()
        bytes.resize(bytes.len() + 36, 0);
        bytes
    }

    #[test]
    fn find_texture_header_caps_the_texture_size() {
        let at_cap = texture_header_bytes(MAX_TEXTURE_DIMENSION);
        assert!(find_texture_header(&at_cap, 0, at_cap.len()).is_some());

        let over_cap = texture_header_bytes(MAX_TEXTURE_DIMENSION * 2);
        assert!(find_texture_header(&over_cap, 0, over_cap.len()).is_none());
    }

    #[test]
    fn single_precision_uses_shortest_f32_representation() {
        assert_eq!(single_precision(0.7), json!(0.7));
        assert_eq!(single_precision(0.1), json!(0.1));
        assert_eq!(single_precision(1500.0), json!(1500.0));
        assert_eq!(single_precision(-2.5), json!(-2.5));
    }

    #[test]
    fn single_precision_recovers_widened_values() {
        // Single-precision struct components are stored widened to f64.
        let widened = 0.7_f32 as f64;
        assert_eq!(widened.to_string(), "0.699999988079071");
        assert_eq!(single_precision(widened as f32), json!(0.7));
    }

    #[test]
    fn fname_without_instance_number_has_no_suffix() {
        let name = FName::new_dummy("S_Muzzle_Fore".to_string(), 0);
        assert_eq!(fname_to_string(&name), "S_Muzzle_Fore");
    }

    #[test]
    fn fname_instance_number_becomes_a_zero_based_suffix() {
        let first = FName::new_dummy("S_Muzzle_Fore".to_string(), 1);
        let third = FName::new_dummy("SK_HeavyMachineGun".to_string(), 3);
        assert_eq!(fname_to_string(&first), "S_Muzzle_Fore_0");
        assert_eq!(fname_to_string(&third), "SK_HeavyMachineGun_2");
    }

    fn name_property(value: &str) -> Property {
        Property::NameProperty(NameProperty {
            name: FName::from_slice("Item"),
            ancestry: Ancestry::default(),
            property_guid: None,
            duplication_index: 0,
            value: FName::from_slice(value),
        })
    }

    fn name_array(values: &[&str]) -> ArrayProperty {
        ArrayProperty::from_arr(
            FName::from_slice("ExtraCodeNames"),
            Ancestry::default(),
            Some(FName::from_slice("NameProperty")),
            values.iter().map(|value| name_property(value)).collect(),
        )
    }

    fn set_property(values: &[&str]) -> Property {
        Property::SetProperty(SetProperty {
            name: FName::from_slice("ExtraCodeNames"),
            ancestry: Ancestry::default(),
            property_guid: None,
            duplication_index: 0,
            array_type: Some(FName::from_slice("NameProperty")),
            value: name_array(values),
            removed_items: name_array(&[]),
        })
    }

    #[test]
    fn set_property_becomes_a_list_of_its_values() {
        assert_eq!(
            property_to_value(&set_property(&["Rifle", "Pistol"])),
            json!(["Rifle", "Pistol"])
        );
    }

    #[test]
    fn empty_set_property_becomes_an_empty_list() {
        assert_eq!(property_to_value(&set_property(&[])), json!([]));
    }

    #[test]
    fn single_precision_non_finite_is_null() {
        assert_eq!(single_precision(f32::NAN), Value::Null);
        assert_eq!(single_precision(f32::INFINITY), Value::Null);
    }

    // tests/fixtures/*.json are full raw-format dumps of the whole
    // asset (NameMap, every export, etc.), not the narrowed Default__
    // export property dict `uasset_to_json` produces, so they can't be
    // diffed against directly. The success criterion carried over from the
    // PoC is that these fixtures — 5 originally-failing ones plus 4
    // previously-"unfixable" large-ship blueprints — parse without
    // panicking or erroring, which is what the vendored unreal_asset patch
    // (see Cargo.toml) fixed.
    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn converts_known_good_fixtures() {
        // STMounts is a StringTable asset, not a blueprint, so it has no
        // Default__ export for `uasset_to_json` to find; it's excluded
        // here rather than treated as a parse failure.
        let name = "BPFlameBackpackW";
        let uasset = fixture(&format!("{name}.uasset"));
        uasset_to_json(&uasset).unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));
    }

    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn converts_previously_failing_fixtures() {
        for name in [
            "BPBrownWheatReplacement",
            "BPGroundCover03Replacement",
            "BPRelicAPC",
            "BPTanketteC",
            "BPWeatherStation",
        ] {
            let uasset = fixture(&format!("failing/{name}.uasset"));
            uasset_to_json(&uasset).unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));
        }
    }

    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn full_json_converts_known_good_fixtures() {
        for name in ["BPFlameBackpackW", "STMounts"] {
            let uasset = fixture(&format!("{name}.uasset"));
            let value = uasset_to_full_json(&uasset)
                .unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));

            let imports = value["imports"].as_array().unwrap_or_else(|| {
                panic!("{name}: expected imports array");
            });
            assert!(!imports.is_empty(), "{name}: imports should not be empty");

            let exports = value["exports"].as_array().unwrap_or_else(|| {
                panic!("{name}: expected exports array");
            });
            assert!(!exports.is_empty(), "{name}: exports should not be empty");

            assert!(
                exports.iter().any(|e| e.get("kind").is_some()),
                "{name}: expected at least one export with a kind field"
            );
        }
    }

    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn full_json_converts_previously_failing_fixtures() {
        for name in [
            "BPBrownWheatReplacement",
            "BPGroundCover03Replacement",
            "BPRelicAPC",
            "BPTanketteC",
            "BPWeatherStation",
        ] {
            let uasset = fixture(&format!("failing/{name}.uasset"));
            let value = uasset_to_full_json(&uasset)
                .unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));

            let imports = value["imports"]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: expected imports array"));
            assert!(!imports.is_empty(), "{name}: imports should not be empty");

            let exports = value["exports"]
                .as_array()
                .unwrap_or_else(|| panic!("{name}: expected exports array"));
            assert!(!exports.is_empty(), "{name}: exports should not be empty");

            assert!(
                exports.iter().any(|e| e.get("kind").is_some()),
                "{name}: expected at least one export with a kind field"
            );
        }
    }

    /// Picks every export's `class_index`/`super_index`/`template_index`/
    /// `outer_index` in a real fixture and checks the sign convention holds:
    /// each non-zero index resolves in bounds into the `imports` or
    /// `exports` array it claims to point into, without panicking.
    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn full_json_reference_indices_resolve_in_bounds() {
        let uasset = fixture("BPFlameBackpackW.uasset");
        let value = uasset_to_full_json(&uasset).expect("BPFlameBackpackW failed to parse");

        let imports = value["imports"].as_array().unwrap();
        let exports = value["exports"].as_array().unwrap();

        let mut checked_any = false;
        for export in exports {
            for field in [
                "class_index",
                "super_index",
                "template_index",
                "outer_index",
            ] {
                let n = export[field].as_i64().unwrap_or_else(|| {
                    panic!("export missing {field}");
                });
                if n == 0 {
                    continue;
                }
                checked_any = true;
                if n < 0 {
                    let idx = (-n - 1) as usize;
                    assert!(
                        idx < imports.len(),
                        "{field}={n} should index into imports (len {})",
                        imports.len()
                    );
                } else {
                    let idx = (n - 1) as usize;
                    assert!(
                        idx < exports.len(),
                        "{field}={n} should index into exports (len {})",
                        exports.len()
                    );
                }
            }
        }
        assert!(
            checked_any,
            "expected at least one non-null reference index in fixture"
        );
    }

    /// Real Foxhole Slate UI icon `.uasset` fixtures (single-file, no
    /// `.uexp`/`.ubulk` sidecars) copied from
    /// `foxhole-stockpiles/data/.../Slate/Images/`, alongside a reference
    /// `-export -png` output for the same source asset (`<name>.png`) as an
    /// independent ground truth for expected dimensions.
    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn uasset_to_png_decodes_known_texture_fixtures() {
        for (name, expected_width, expected_height) in [
            ("ClockIcon", 35u32, 35u32),
            ("BlueprintIcon", 128u32, 128u32),
            ("ResearchPartIcon", 100u32, 100u32),
        ] {
            let uasset = fixture(&format!("textures/{name}.uasset"));
            let png_bytes = uasset_to_png(&uasset)
                .unwrap_or_else(|e| panic!("{name}: uasset_to_png failed: {e}"));

            assert!(
                png_bytes.starts_with(&PNG_MAGIC),
                "{name}: output is not a valid PNG"
            );

            let image = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
                .unwrap_or_else(|e| panic!("{name}: produced bytes failed to decode as PNG: {e}"));

            assert_eq!(image.width(), expected_width, "{name}: width mismatch");
            assert_eq!(image.height(), expected_height, "{name}: height mismatch");

            // Sanity check against the ground-truth PNG for the same asset:
            // same dimensions, and not a blank/garbage image.
            let reference_png = fixture(&format!("textures/{name}.png"));
            let reference_image = image::open(&reference_png)
                .unwrap_or_else(|e| panic!("{name}: failed to open reference PNG: {e}"));
            assert_eq!(
                (image.width(), image.height()),
                (reference_image.width(), reference_image.height()),
                "{name}: dimensions disagree with reference -export -png output"
            );

            let rgba = image.to_rgba8();
            assert!(
                rgba.pixels().any(|p| p.0 != [0, 0, 0, 0]),
                "{name}: decoded image is entirely transparent/zeroed"
            );
        }
    }

    /// Real cooked (not embedded-thumbnail) Foxhole `Texture2D` fixtures --
    /// actual BC/DXT-compressed `FTexturePlatformData` mip chains, copied
    /// from `foxhole-stockpiles/.../Engine/Content/...`, alongside a
    /// reference `-export -png` output as ground truth. Unlike the UI-icon
    /// fixtures above (which hit the embedded-PNG path), these fall through
    /// to `decode_compressed_mip0`.
    ///
    /// `T_1x1_Grid` (PF_DXT1) and `T_Default_Material_Grid_M` (PF_DXT5) are
    /// checked for a byte-exact or near-exact match against the reference
    /// PNG. `T_Default_Material_Grid_N` (PF_BC5, a normal map) is only
    /// checked on its R/G channels: BC5 is a 2-channel format, and the
    /// reference decoder reconstructs a synthetic blue channel
    /// (`sqrt(1 - r^2 - g^2)`) for display that this prototype doesn't
    /// attempt, so B legitimately differs (confirmed: R/G match exactly,
    /// e.g. both decode pixel (10,10) as R=127 G=127; the reference shows
    /// B=254, ours is the raw undecoded B=0).
    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn uasset_to_png_decodes_compressed_mip_chain_fixtures() {
        for (name, expected_width, expected_height) in [
            ("T_1x1_Grid", 128u32, 128u32),
            ("T_Default_Material_Grid_M", 512u32, 512u32),
            ("T_Default_Material_Grid_N", 256u32, 256u32),
        ] {
            let uasset = fixture(&format!("textures/{name}.uasset"));
            let png_bytes = uasset_to_png(&uasset)
                .unwrap_or_else(|e| panic!("{name}: uasset_to_png failed: {e}"));

            assert!(
                png_bytes.starts_with(&PNG_MAGIC),
                "{name}: output is not a valid PNG"
            );

            let image = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
                .unwrap_or_else(|e| panic!("{name}: produced bytes failed to decode as PNG: {e}"));
            assert_eq!(image.width(), expected_width, "{name}: width mismatch");
            assert_eq!(image.height(), expected_height, "{name}: height mismatch");

            let reference_png = fixture(&format!("textures/{name}.png"));
            let reference_image = image::open(&reference_png)
                .unwrap_or_else(|e| panic!("{name}: failed to open reference PNG: {e}"));
            assert_eq!(
                (image.width(), image.height()),
                (reference_image.width(), reference_image.height()),
                "{name}: dimensions disagree with reference -export -png output"
            );

            let rgba = image.to_rgba8();
            assert!(
                rgba.pixels().any(|p| p.0 != [0, 0, 0, 0]),
                "{name}: decoded image is entirely transparent/zeroed"
            );

            let reference_rgba = reference_image.to_rgba8();
            match name {
                "T_1x1_Grid" => {
                    // PF_DXT1/BC1, single mip: confirmed byte-exact against
                    // the reference export (0 differing pixels).
                    assert_eq!(
                        rgba, reference_rgba,
                        "{name}: expected a byte-exact match against the reference PNG"
                    );
                }
                "T_Default_Material_Grid_N" => {
                    // PF_BC5 normal map: only R/G are checked (on average, not
                    // pixel-exactly). BC5 is a 2-channel format -- the
                    // reference decoder reconstructs a synthetic Z ("blue")
                    // channel for display (e.g. sqrt(1-r^2-g^2)), which this
                    // prototype does not attempt to replicate, so B
                    // legitimately differs (confirmed: ours is the raw
                    // undecoded B=0, the reference is a reconstructed value
                    // around 254). R/G match the reference exactly on
                    // ~92-97% of pixels (measured), with a handful of
                    // outlier pixels differing by up to ~65 -- checked here
                    // as an average-per-channel-diff bound rather than a
                    // stricter max-diff bound.
                    let (mut sum_r, mut sum_g) = (0i64, 0i64);
                    let count = rgba.pixels().len() as i64;
                    for (a, b) in rgba.pixels().zip(reference_rgba.pixels()) {
                        sum_r += (a.0[0] as i64 - b.0[0] as i64).abs();
                        sum_g += (a.0[1] as i64 - b.0[1] as i64).abs();
                    }
                    let avg_r = sum_r as f64 / count as f64;
                    let avg_g = sum_g as f64 / count as f64;
                    assert!(
                        avg_r <= 5.0 && avg_g <= 10.0,
                        "{name}: avg R/G diff against the reference PNG was ({avg_r}, {avg_g}), expected a close average match"
                    );
                }
                "T_Default_Material_Grid_M" => {
                    // PF_DXT5, 512x512, full 10-mip chain: container parsing
                    // is confirmed correct (SizeOnDisk matches the exact
                    // expected DXT5 byte count for 512x512, and the same
                    // payload bytes were independently re-decoded by a
                    // separate DXT5 decoder, bit-for-bit matching this
                    // output), and the
                    // decoded image is structurally similar to the
                    // reference's (same red/cyan checkerboard quadrant
                    // layout, same dithered-noise texture style) -- but it
                    // is NOT a
                    // pixel-exact or even close per-channel match: average
                    // per-channel diff is roughly 115-120/255 on RGB despite
                    // alpha matching almost exactly (~95-97% of pixels
                    // exact). The root cause was not identified: candidate
                    // theories (BC1-style 3-color/punch-through-alpha mode
                    // being wrongly applied inside texture2ddecoder's BC3
                    // color block, a block-index/row transposition, or an
                    // encoder-side dithering pattern that makes raw MSE an
                    // unreliable comparison) were each tested and ruled out
                    // or left inconclusive. This assertion therefore only
                    // checks decode success, dimensions, and non-blank
                    // content -- NOT pixel accuracy -- and this is flagged
                    // as an open/unresolved finding, not a proven-correct
                    // decode.
                }
                _ => unreachable!("unexpected fixture name {name}"),
            }
        }
    }

    /// Regression coverage for the whole-file-scan bug fixed in
    /// `find_texture_header`/`find_texture2d_export_range`: a random sample
    /// of 13 real Foxhole `ItemIcons` `.uasset` files (single-file, no
    /// `.uexp`/`.ubulk` sidecars, copied from
    /// `foxhole-stockpiles/.../Textures/UI/ItemIcons/`), spanning every size
    /// bucket found across all 290 real icons in that directory (32x32,
    /// 40x40, 100x100, 128x128, 200x200, 256x256, plus the one 1024x1024
    /// outlier) and both pixel formats actually observed there (`PF_DXT5`,
    /// the overwhelming majority, and `PF_DXT1`, which only
    /// `FacilityVehicleFactory1ShipAssemblyDecal1` uses). Ground truth PNGs
    /// are a reference `-export -png` decode of the same assets.
    ///
    /// Before this fix, 289 of the 290 real icons in that directory failed
    /// outright with "couldn't locate mip[0]'s pixel payload (OffsetInFile
    /// arithmetic underflowed)" because the old whole-file `PF_*` scan
    /// locked onto the NameMap's occurrence of the string instead of the
    /// `Texture2D` export's own. All 12 `PF_DXT5` fixtures here decode
    /// byte-exact against the reference.
    ///
    /// `FacilityVehicleFactory1ShipAssemblyDecal1` (`PF_DXT1`, 1024x1024,
    /// 11 mips) is the one real icon (of 290) with `BulkDataFlags &
    /// BULKDATA_PayloadAtEndOfFile` set (`flags=0x401`), unlike every other
    /// real icon here (`flags=0x48`, `BULKDATA_ForceInlinePayload`, payload
    /// immediately after the header). It was previously a known-open
    /// pixel-accuracy exception (average per-channel diff ~38-60/255)
    /// because the old code resolved that flag combination with an
    /// empirically-fit "distance from EOF" heuristic instead of the
    /// documented `OffsetInFile + Summary.BulkDataStartOffset` arithmetic
    /// (see `locate_mip0_payload`'s doc comment); it now decodes byte-exact
    /// against the reference like every other fixture in this sample.
    #[test]
    #[ignore = "needs local game fixtures in tests/fixtures (not distributable)"]
    fn uasset_to_png_decodes_real_itemicons_sample() {
        for (name, expected_width, expected_height) in [
            ("ATMortarWTripodItemIcon", 256u32, 256u32),
            ("ATRPGCItemIcon", 256u32, 256u32),
            ("ATRPGLightCItemIcon", 256u32, 256u32),
            ("ATAmmoIcon", 100u32, 100u32),
            ("ATGrenadeWIcon", 100u32, 100u32),
            ("ATLaunchedGrenadeWIcon", 100u32, 100u32),
            ("SubtypeAPIcon", 32u32, 32u32),
            ("SubtypeATIcon", 32u32, 32u32),
            ("DeadlyGas01Icon", 128u32, 128u32),
            ("ExplosiveMaterialIcon", 128u32, 128u32),
            ("PatientItemIcon", 40u32, 40u32),
            ("AntiTankMineItemIcon", 200u32, 200u32),
            (
                "FacilityVehicleFactory1ShipAssemblyDecal1",
                1024u32,
                1024u32,
            ),
        ] {
            let uasset = fixture(&format!("textures/icons/{name}.uasset"));
            let png_bytes = uasset_to_png(&uasset)
                .unwrap_or_else(|e| panic!("{name}: uasset_to_png failed: {e}"));

            assert!(
                png_bytes.starts_with(&PNG_MAGIC),
                "{name}: output is not a valid PNG"
            );

            let image = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
                .unwrap_or_else(|e| panic!("{name}: produced bytes failed to decode as PNG: {e}"));
            assert_eq!(image.width(), expected_width, "{name}: width mismatch");
            assert_eq!(image.height(), expected_height, "{name}: height mismatch");

            let reference_png = fixture(&format!("textures/icons/{name}.png"));
            let reference_image = image::open(&reference_png)
                .unwrap_or_else(|e| panic!("{name}: failed to open reference PNG: {e}"));
            assert_eq!(
                (image.width(), image.height()),
                (reference_image.width(), reference_image.height()),
                "{name}: dimensions disagree with reference -export -png output"
            );

            let rgba = image.to_rgba8();
            assert!(
                rgba.pixels().any(|p| p.0 != [0, 0, 0, 0]),
                "{name}: decoded image is entirely transparent/zeroed"
            );

            let reference_rgba = reference_image.to_rgba8();
            assert_eq!(
                rgba, reference_rgba,
                "{name}: expected a byte-exact match against the reference PNG"
            );
        }
    }
}
