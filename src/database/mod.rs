//! Template database assembly: icon template generation and HDF5 output.
//!
//! This module owns the *write* side of the HDF5 template-database format
//! that `fs-ocr` (a sibling Rust crate) owns the *read* side of. The schema
//! here is **manually kept in lockstep** with `fs-ocr/src/template/database.rs`
//! (no shared crate exists yet — see the module-level doc comment in
//! [`hdf5_writer`]). Template pixels and pHashes are bit-exact with Pillow's
//! LANCZOS resize and OpenCV's grayscale conversion + `INTER_AREA` resize.
//!
//! Submodules follow the same split as `crate::catalog`: this module (`mod.rs`)
//! holds pure, fully-testable logic (catalog-entry -> template metadata
//! derivation, resize, phash); [`hdf5_writer`] is the thin real-HDF5-I/O
//! layer; [`pipeline`] is the filesystem-backed integration layer wiring
//! both together into the `add-mod` CLI command.

pub mod hdf5_writer;
pub mod pillow_resample;
pub mod pipeline;

use image::{DynamicImage, RgbaImage};
use serde_json::Value;

use crate::catalog::stockpile_classifier::StockpileCategory;
use pillow_resample::{flatten_on_black_to_bgr, resize_lanczos_rgba};

/// Item faction classification, mirroring `fs-ocr`'s `ItemFaction` enum
/// (`fs-ocr/src/enums/item_faction.rs`) **exactly**, including discriminant
/// values — these are written to HDF5 as a `u8` index and must match on both
/// the write (here) and read (`fs-ocr`) sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ItemFaction {
    #[default]
    Neutral = 0,
    Colonials = 1,
    Wardens = 2,
}

impl ItemFaction {
    /// Parses a faction name (case-insensitive, `EFactionId::` prefix
    /// optional, `c`/`w` shorthands); anything else is `Neutral`. Matches
    /// `fs-ocr`'s `ItemFaction::from_string`.
    pub fn from_string(value: Option<&str>) -> Self {
        match value {
            None => ItemFaction::Neutral,
            Some(s) => match s.trim().to_lowercase().as_str() {
                "efactionid::colonials" | "colonials" | "c" => ItemFaction::Colonials,
                "efactionid::wardens" | "wardens" | "w" => ItemFaction::Wardens,
                _ => ItemFaction::Neutral,
            },
        }
    }

    pub fn as_index(self) -> u8 {
        self as u8
    }

    /// Lowercase name, the same spelling `modify-icon` accepts.
    pub fn name(self) -> &'static str {
        match self {
            ItemFaction::Neutral => "neutral",
            ItemFaction::Colonials => "colonials",
            ItemFaction::Wardens => "wardens",
        }
    }

    /// Inverse of [`ItemFaction::as_index`], used when reading templates back
    /// out of an existing HDF5 database (`add-mod`'s merge step).
    pub fn from_index(value: u8) -> Self {
        match value {
            1 => ItemFaction::Colonials,
            2 => ItemFaction::Wardens,
            _ => ItemFaction::Neutral,
        }
    }
}

/// Item category classification, mirroring `fs-ocr`'s `ItemCategory` enum
/// (`fs-ocr/src/enums/item_category.rs`) exactly, including discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ItemCategory {
    Item = 0,
    Vehicle = 1,
    Shippable = 2,
    #[default]
    Invalid = 3,
}

impl ItemCategory {
    pub fn as_index(self) -> u8 {
        self as u8
    }

    /// Lowercase name, the same spelling `modify-icon` accepts.
    pub fn name(self) -> &'static str {
        match self {
            ItemCategory::Item => "item",
            ItemCategory::Vehicle => "vehicle",
            ItemCategory::Shippable => "shippable",
            ItemCategory::Invalid => "invalid",
        }
    }

    /// Inverse of [`ItemCategory::as_index`], used when reading templates
    /// back out of an existing HDF5 database (`add-mod`'s merge step).
    pub fn from_index(value: u8) -> Self {
        match value {
            0 => ItemCategory::Item,
            1 => ItemCategory::Vehicle,
            2 => ItemCategory::Shippable,
            _ => ItemCategory::Invalid,
        }
    }
}

/// A catalog item's fields relevant to template generation, derived from a
/// single `catalog.json` entry, in the shape this crate's own
/// `build-catalog` command produces (see `crate::catalog::catalog_assembler`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIconItem {
    pub code: String,
    pub faction: ItemFaction,
    pub category: ItemCategory,
    /// Normalized (version-suffix-stripped) object path to the item's icon
    /// `Texture2D` asset, e.g. `War/Content/Textures/UI/ItemIcons/T_Rifle`.
    pub icon_path: String,
    /// Normalized object path to the item's subtype-icon overlay asset (the
    /// catalog's `SubTypeIcon` field), if any. Blended into the top-left
    /// corner of the base icon by [`build_icon_templates`].
    pub subicon_path: Option<String>,
    /// Whether this item gets a second, `crated=true` template with the
    /// shared crate overlay blended in — the catalog's `bIsCratable` flag.
    pub cratable: bool,
}

/// Strips a raw-asset-dump-style version suffix (e.g. the trailing `.0` in
/// `War/Content/Foo.0`) from an object path: a trailing `.` followed by
/// one or more digits is removed.
pub fn normalize_icon_path(path: &str) -> String {
    match path.rfind('.') {
        Some(dot)
            if path[dot + 1..].chars().all(|c| c.is_ascii_digit()) && dot + 1 < path.len() =>
        {
            path[..dot].to_string()
        }
        _ => path.to_string(),
    }
}

/// Fails unless `value` can name a template in the database: not empty, and
/// free of `\0`, which HDF5 strings can't hold. `what` names the value in the
/// error. Checked before any work, so a bad catalog code or mod name fails at
/// once rather than when the database is finally written.
pub fn check_template_name(what: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{what} is empty");
    }
    if value.contains('\0') {
        anyhow::bail!("{what} {value:?} contains a NUL character");
    }
    Ok(())
}

/// Derives a [`CatalogIconItem`] from one `catalog.json` entry (a
/// `serde_json::Value` object, matching the shape emitted by
/// `crate::catalog::catalog_assembler::clean_entry`). Returns `None` when the
/// entry has no usable `CodeName` (icons are never generated for items
/// without a code).
pub fn derive_catalog_icon_item(entry: &Value) -> Option<CatalogIconItem> {
    let code = entry.get("CodeName").and_then(Value::as_str).unwrap_or("");
    if code.is_empty() {
        return None;
    }

    let faction = ItemFaction::from_string(entry.get("FactionVariant").and_then(Value::as_str));
    let category = determine_item_category(entry);
    let icon_path = entry
        .get("Icon")
        .and_then(Value::as_str)
        .map(normalize_icon_path)
        .unwrap_or_default();
    let subicon_path = entry
        .get("SubTypeIcon")
        .and_then(Value::as_str)
        .map(normalize_icon_path);
    let cratable = determine_cratable(entry);

    Some(CatalogIconItem {
        code: code.to_string(),
        faction,
        category,
        icon_path,
        subicon_path,
        cratable,
    })
}

/// Determines whether an item gets a `crated=true` template, from the
/// catalog's resolved `bIsCratable` flag.
fn determine_cratable(entry: &Value) -> bool {
    entry.get("bIsCratable") == Some(&Value::Bool(true))
}

/// Maps the catalog's `StockpileCategory` to an [`ItemCategory`]
/// (`Items` -> `Item`, `Vehicles` -> `Vehicle`, `Structures` ->
/// `Shippable`); missing or unknown categories are `Invalid`.
fn determine_item_category(entry: &Value) -> ItemCategory {
    match entry
        .get("StockpileCategory")
        .and_then(Value::as_str)
        .and_then(StockpileCategory::from_string)
    {
        Some(StockpileCategory::Items) => ItemCategory::Item,
        Some(StockpileCategory::Vehicles) => ItemCategory::Vehicle,
        Some(StockpileCategory::Structures) => ItemCategory::Shippable,
        None => ItemCategory::Invalid,
    }
}

/// All supported vertical resolutions for template matching, copied verbatim
/// from `fs-ocr`'s `SUPPORTED_RESOLUTIONS` (`fs-ocr/src/constants.rs`) — the
/// HDF5 file must have exactly one group per one of these values for
/// `fs-ocr`'s resolution lookup (`find_closest_resolution`) to work as
/// expected.
pub const SUPPORTED_RESOLUTIONS: [i32; 16] = [
    664, 720, 768, 800, 864, 900, 960, 992, 1024, 1050, 1080, 1200, 1440, 1536, 1600, 2160,
];

/// Validates the resolutions a caller asked for: none means every entry of
/// [`SUPPORTED_RESOLUTIONS`]; otherwise each one must be supported, and the
/// result is sorted and deduplicated.
pub fn resolve_resolutions(requested: &[i32]) -> anyhow::Result<Vec<i32>> {
    if requested.is_empty() {
        return Ok(SUPPORTED_RESOLUTIONS.to_vec());
    }
    if let Some(unsupported) = requested
        .iter()
        .find(|r| !SUPPORTED_RESOLUTIONS.contains(r))
    {
        anyhow::bail!(
            "unsupported resolution {unsupported}; supported: {}",
            SUPPORTED_RESOLUTIONS.map(|r| r.to_string()).join(", ")
        );
    }
    let mut resolutions = requested.to_vec();
    resolutions.sort_unstable();
    resolutions.dedup();
    Ok(resolutions)
}

/// Icon scaling factor: 64px icons at 2160p, scaled proportionally for
/// lower resolutions.
const ICON_SCALING_FACTOR: f64 = 64.0 / 2160.0;

/// Computes the square icon size (in pixels) for a given vertical resolution.
/// The scaled size is truncated, not rounded.
pub fn icon_size_for_resolution(resolution_height: i32) -> u32 {
    (ICON_SCALING_FACTOR * resolution_height as f64) as u32
}

/// Fraction of the icon's side length that a subicon/crate overlay occupies,
/// and the blend weight applied on top of the overlay's own alpha.
const SUBICON_ASPECT_RATIO: f64 = 7.0 / 16.0;
const SUBICON_ALPHA: f32 = 0.75;

/// Default color-tint multiplier/offset applied to every subicon/crate
/// overlay pixel with alpha above [`TINT_MIN_ALPHA`] before resizing
/// (`result = pixel * mult / 255 + offset`), in `[blue, green, red]` order.
const OVERLAY_TINT_MULTIPLIER_BGR: [f32; 3] = [145.0, 152.0, 154.0];
const OVERLAY_TINT_OFFSET_BGR: [f32; 3] = [82.0, 87.0, 89.0];
/// Alpha above which an overlay pixel gets the color tint.
const TINT_MIN_ALPHA: u8 = 10;

/// Object path (no `.uasset` extension, no version suffix) of the single
/// shared crate-overlay texture blended onto every cratable item's icon —
/// this is one fixed texture, not a per-item asset.
pub const CRATE_ICON_OBJECT_PATH: &str = "War/Content/Textures/UI/Menus/IconFilterCrates";

/// Alpha above which a subicon pixel counts as visible.
const BLANK_SUBICON_VISIBLE_ALPHA: u8 = 10;
/// Visible-pixel fraction below which a subicon counts as transparent.
const BLANK_SUBICON_VISIBLE_RATIO: f64 = 0.05;
/// Channel standard deviation below which visible pixels form a solid color.
const BLANK_SUBICON_SOLID_STD: f64 = 15.0;
/// Channel mean below which a solid-color subicon counts as a gray placeholder.
const BLANK_SUBICON_DARK_MEAN: f64 = 150.0;

/// Whether `subicon` is a blank placeholder that some mods ship to remove the
/// subicon: (almost) fully transparent, or a solid dark/gray square. Such a
/// subicon is not blended at all.
pub fn is_blank_subicon(subicon: &DynamicImage) -> bool {
    let rgba = subicon.to_rgba8();
    let total = (rgba.width() as usize) * (rgba.height() as usize);
    if total == 0 {
        return true;
    }

    // One pass of exact integer sums over the visible pixels' channels,
    // rather than collecting them: a large subicon would otherwise need 24
    // bytes per pixel just for the statistics.
    let (mut pixels, mut sum, mut sum_sq) = (0u64, 0u64, 0u128);
    for px in rgba
        .pixels()
        .filter(|px| px[3] > BLANK_SUBICON_VISIBLE_ALPHA)
    {
        pixels += 1;
        for &channel in &px.0[..3] {
            sum += u64::from(channel);
            sum_sq += u128::from(channel) * u128::from(channel);
        }
    }
    if (pixels as f64 / total as f64) < BLANK_SUBICON_VISIBLE_RATIO {
        return true;
    }

    let count = u128::from(pixels * 3);
    let mean = sum as f64 / count as f64;
    // count² · variance = count · Σx² − (Σx)², exact in integers.
    let variance =
        (count * sum_sq - u128::from(sum) * u128::from(sum)) as f64 / (count as f64 * count as f64);
    variance.sqrt() < BLANK_SUBICON_SOLID_STD && mean < BLANK_SUBICON_DARK_MEAN
}

/// Resizes `image` to `size x size` with Pillow's LANCZOS and alpha-composites
/// it onto a black background, returning row-major BGR bytes.
fn resize_and_flatten_to_bgr(image: &RgbaImage, size: u32) -> Vec<u8> {
    flatten_on_black_to_bgr(&resize_lanczos_rgba(image, size, size))
}

/// Applies the overlay color tint to every pixel with alpha above
/// [`TINT_MIN_ALPHA`], computed in `f32` (numpy float32 semantics) and
/// truncated back to `u8`.
fn tint_overlay(overlay: &RgbaImage) -> RgbaImage {
    let mut tinted = overlay.clone();
    for px in tinted.pixels_mut().filter(|px| px[3] > TINT_MIN_ALPHA) {
        // `px` is RGBA; the tint tables are in BGR order.
        for (rgba_index, bgr_index) in [(2, 0), (1, 1), (0, 2)] {
            let value = px[rgba_index] as f32 * OVERLAY_TINT_MULTIPLIER_BGR[bgr_index] / 255.0
                + OVERLAY_TINT_OFFSET_BGR[bgr_index];
            px[rgba_index] = value.clamp(0.0, 255.0) as u8;
        }
    }
    tinted
}

/// Resizes `tinted` (an overlay already passed through [`tint_overlay`]) to
/// `size * SUBICON_ASPECT_RATIO` with Pillow's LANCZOS and alpha-blends it
/// (its alpha weighted by `SUBICON_ALPHA`) into a corner of `base`
/// (`size x size` BGR bytes), in place. `top_left` picks the subicon's
/// corner; the crate overlay uses the opposite (bottom-right) corner. The
/// blend is computed in `f32` (numpy float32 semantics).
fn blend_overlay_bgr(base: &mut [u8], size: u32, tinted: &RgbaImage, top_left: bool) {
    let overlay_size = ((size as f64) * SUBICON_ASPECT_RATIO) as u32;
    if overlay_size == 0 || overlay_size > size {
        return;
    }
    let resized = resize_lanczos_rgba(tinted, overlay_size, overlay_size);
    let (x0, y0) = if top_left {
        (0, 0)
    } else {
        (size - overlay_size, size - overlay_size)
    };

    for (ox, oy, px) in resized.enumerate_pixels() {
        let alpha = px[3] as f32 / 255.0 * SUBICON_ALPHA;
        let idx = (((y0 + oy) * size + (x0 + ox)) * 3) as usize;
        for (c, &value) in [px[2], px[1], px[0]].iter().enumerate() {
            let blended = (1.0 - alpha) * base[idx + c] as f32 + alpha * value as f32;
            base[idx + c] = blended as u8;
        }
    }
}

/// `image` at its own size with `subicon` drawn over its top-left corner the
/// way the templates get it (tinted, `SUBICON_ASPECT_RATIO` of the icon's
/// shorter side, alpha weighted by `SUBICON_ALPHA`), but alpha-composited
/// onto the icon instead of a black background, so the icon's transparency
/// is kept. A blank placeholder subicon (see [`is_blank_subicon`]) is not
/// drawn.
pub fn composite_subicon(image: &DynamicImage, subicon: &DynamicImage) -> RgbaImage {
    let mut base = image.to_rgba8();
    let overlay_size = (base.width().min(base.height()) as f64 * SUBICON_ASPECT_RATIO) as u32;
    if overlay_size == 0 || is_blank_subicon(subicon) {
        return base;
    }
    let resized = resize_lanczos_rgba(
        &tint_overlay(&subicon.to_rgba8()),
        overlay_size,
        overlay_size,
    );

    for (x, y, px) in resized.enumerate_pixels() {
        let overlay_alpha = px[3] as f32 / 255.0 * SUBICON_ALPHA;
        let dest = base.get_pixel_mut(x, y);
        let base_alpha = dest[3] as f32 / 255.0;
        let out_alpha = overlay_alpha + base_alpha * (1.0 - overlay_alpha);
        if out_alpha <= 0.0 {
            continue;
        }
        for c in 0..3 {
            let value = (overlay_alpha * px[c] as f32
                + base_alpha * (1.0 - overlay_alpha) * dest[c] as f32)
                / out_alpha;
            dest[c] = value.round().clamp(0.0, 255.0) as u8;
        }
        dest[3] = (out_alpha * 255.0).round() as u8;
    }
    base
}

/// Size of the pHash grid (8x8 = 64 bits). Copied from `fs-ocr`'s
/// `phash.rs::HASH_SIZE`.
const HASH_SIZE: usize = 8;

/// Computes a 64-bit perceptual hash for a BGR image, bit-exact with
/// OpenCV:
/// 1. `cv2.cvtColor(..., cv2.COLOR_BGR2GRAY)` grayscale (fixed-point).
/// 2. `cv2.resize(gray, (8, 8), interpolation=cv2.INTER_AREA)`.
/// 3. Strictly-greater-than-average thresholding (not `>=`).
pub fn compute_phash(image: &[u8], width: usize, height: usize) -> u64 {
    if image.is_empty() || width == 0 || height == 0 {
        return 0;
    }

    let grayscale = bgr_to_grayscale(image, width, height);
    let resized = resize_inter_area(&grayscale, width, height, HASH_SIZE, HASH_SIZE);

    let sum: f64 = resized.iter().map(|&x| x as f64).sum();
    let avg = sum / 64.0;

    let mut hash: u64 = 0;
    for (i, &pixel) in resized.iter().enumerate() {
        if (pixel as f64) > avg {
            hash |= 1 << (63 - i);
        }
    }

    hash
}

/// OpenCV's fixed-point `COLOR_BGR2GRAY` coefficients (14-bit shift).
const GRAY_SHIFT: u32 = 14;
const GRAY_B: u32 = 1868;
const GRAY_G: u32 = 9617;
const GRAY_R: u32 = 4899;

/// `cv2.cvtColor(image, cv2.COLOR_BGR2GRAY)` for 8-bit BGR, bit-exact.
fn bgr_to_grayscale(image: &[u8], width: usize, height: usize) -> Vec<u8> {
    image
        .as_chunks::<3>()
        .0
        .iter()
        .take(width * height)
        .map(|&[b, g, r]| {
            let weighted = b as u32 * GRAY_B + g as u32 * GRAY_G + r as u32 * GRAY_R;
            ((weighted + (1 << (GRAY_SHIFT - 1))) >> GRAY_SHIFT) as u8
        })
        .collect()
}

/// Rounds like OpenCV's `saturate_cast<uchar>(float)` (ties to even).
fn saturate_u8(value: f32) -> u8 {
    value.round_ties_even().clamp(0.0, 255.0) as u8
}

/// One source sample's contribution to one destination index, as in
/// OpenCV's `computeResizeAreaTab`.
struct AreaWeight {
    dst: usize,
    src: usize,
    alpha: f32,
}

/// OpenCV's `computeResizeAreaTab` for one axis.
fn area_weights(src_size: usize, dst_size: usize, scale: f64) -> Vec<AreaWeight> {
    let mut weights = Vec::new();
    for dst in 0..dst_size {
        let fsx1 = dst as f64 * scale;
        let fsx2 = fsx1 + scale;
        let cell = scale.min(src_size as f64 - fsx1);
        let sx2 = (fsx2.floor() as usize).min(src_size - 1);
        let sx1 = (fsx1.ceil() as usize).min(sx2);

        if sx1 as f64 - fsx1 > 1e-3 {
            weights.push(AreaWeight {
                dst,
                src: sx1 - 1,
                alpha: ((sx1 as f64 - fsx1) / cell) as f32,
            });
        }
        for src in sx1..sx2 {
            weights.push(AreaWeight {
                dst,
                src,
                alpha: (1.0 / cell) as f32,
            });
        }
        if fsx2 - sx2 as f64 > 1e-3 {
            weights.push(AreaWeight {
                dst,
                src: sx2,
                alpha: ((fsx2 - sx2 as f64).min(1.0).min(cell) / cell) as f32,
            });
        }
    }
    weights
}

/// `cv2.resize(gray, (out_width, out_height), interpolation=cv2.INTER_AREA)`
/// for a downscale of a single-channel 8-bit image, bit-exact: integer scale
/// factors average whole blocks, fractional ones accumulate the per-axis
/// area weights in `f32`.
fn resize_inter_area(
    image: &[u8],
    in_width: usize,
    in_height: usize,
    out_width: usize,
    out_height: usize,
) -> Vec<u8> {
    let scale_x = in_width as f64 / out_width as f64;
    let scale_y = in_height as f64 / out_height as f64;
    if scale_x.fract() == 0.0 && scale_y.fract() == 0.0 {
        return resize_inter_area_integer(
            image,
            in_width,
            scale_x as usize,
            scale_y as usize,
            out_width,
            out_height,
        );
    }

    let x_weights = area_weights(in_width, out_width, scale_x);
    let y_weights = area_weights(in_height, out_height, scale_y);
    let mut result = vec![0u8; out_width * out_height];
    let mut sum = vec![0f32; out_width];
    let mut row = vec![0f32; out_width];
    let mut prev_dy = y_weights.first().map_or(0, |w| w.dst);

    for y_weight in &y_weights {
        row.iter_mut().for_each(|v| *v = 0.0);
        let src_row = &image[y_weight.src * in_width..(y_weight.src + 1) * in_width];
        for x_weight in &x_weights {
            row[x_weight.dst] += src_row[x_weight.src] as f32 * x_weight.alpha;
        }

        if y_weight.dst != prev_dy {
            for (dx, value) in sum.iter_mut().enumerate() {
                result[prev_dy * out_width + dx] = saturate_u8(*value);
                *value = y_weight.alpha * row[dx];
            }
            prev_dy = y_weight.dst;
        } else {
            for (value, &partial) in sum.iter_mut().zip(&row) {
                *value += y_weight.alpha * partial;
            }
        }
    }
    for (dx, &value) in sum.iter().enumerate() {
        result[prev_dy * out_width + dx] = saturate_u8(value);
    }
    result
}

/// OpenCV's integer-scale `INTER_AREA` path: each output pixel is the block
/// sum times `1 / block_area` in `f32`.
fn resize_inter_area_integer(
    image: &[u8],
    in_width: usize,
    block_width: usize,
    block_height: usize,
    out_width: usize,
    out_height: usize,
) -> Vec<u8> {
    let inverse_area = 1.0f32 / (block_width * block_height) as f32;
    let mut result = vec![0u8; out_width * out_height];
    for out_y in 0..out_height {
        for out_x in 0..out_width {
            let block_sum: u32 = (0..block_height)
                .flat_map(|by| {
                    let row = (out_y * block_height + by) * in_width + out_x * block_width;
                    image[row..row + block_width].iter().map(|&v| v as u32)
                })
                .sum();
            result[out_y * out_width + out_x] = saturate_u8(block_sum as f32 * inverse_area);
        }
    }
    result
}

/// One resolution-specific rendered icon template, ready to be written to an
/// HDF5 group. Mirrors `fs-ocr`'s `IconTemplate` (`fs-ocr/src/template/database.rs`)
/// field-for-field, since that struct's shape is exactly what gets read back.
#[derive(Debug, Clone)]
pub struct IconTemplate {
    /// `size * size * 3` row-major BGR bytes.
    pub image: Vec<u8>,
    pub code: String,
    pub mod_name: String,
    pub faction: ItemFaction,
    pub category: ItemCategory,
    pub crated: bool,
    pub phash: u64,
}

/// Builds the automatically-composited [`IconTemplate`]s for `item` at
/// `resolution_height`: always one `crated=false` template (main icon
/// alpha-composited onto black, with `subicon` — the item's `SubTypeIcon` —
/// tinted and blended into the top-left corner when present), plus a second
/// `crated=true` template when `item.cratable` and `crate_icon` is available,
/// blending the shared crate overlay into the bottom-right corner of the same
/// base.
pub fn build_icon_templates(
    item: &CatalogIconItem,
    mod_name: &str,
    image: &DynamicImage,
    subicon: Option<&DynamicImage>,
    crate_icon: Option<&DynamicImage>,
    resolution_height: i32,
) -> Vec<IconTemplate> {
    PreparedIcon::new(image, subicon, crate_icon).templates(item, mod_name, resolution_height)
}

/// An item's main icon and overlays converted once for every resolution its
/// templates are built at: the icon as RGBA, the overlays with their color
/// tint applied, and a blank placeholder subicon (see [`is_blank_subicon`])
/// dropped.
pub struct PreparedIcon {
    image: RgbaImage,
    subicon: Option<RgbaImage>,
    crate_icon: Option<RgbaImage>,
}

impl PreparedIcon {
    pub fn new(
        image: &DynamicImage,
        subicon: Option<&DynamicImage>,
        crate_icon: Option<&DynamicImage>,
    ) -> Self {
        Self {
            image: image.to_rgba8(),
            subicon: subicon
                .filter(|subicon| !is_blank_subicon(subicon))
                .map(|subicon| tint_overlay(&subicon.to_rgba8())),
            crate_icon: crate_icon.map(|crate_icon| tint_overlay(&crate_icon.to_rgba8())),
        }
    }

    /// The templates [`build_icon_templates`] builds for `item` at
    /// `resolution_height`.
    pub fn templates(
        &self,
        item: &CatalogIconItem,
        mod_name: &str,
        resolution_height: i32,
    ) -> Vec<IconTemplate> {
        let size = icon_size_for_resolution(resolution_height);
        let mut base = resize_and_flatten_to_bgr(&self.image, size);
        if let Some(subicon) = &self.subicon {
            blend_overlay_bgr(&mut base, size, subicon, true);
        }

        let base_phash = compute_phash(&base, size as usize, size as usize);
        let mut templates = vec![IconTemplate {
            image: base.clone(),
            code: item.code.clone(),
            mod_name: mod_name.to_string(),
            faction: item.faction,
            category: item.category,
            crated: false,
            phash: base_phash,
        }];

        if item.cratable {
            if let Some(crate_icon) = &self.crate_icon {
                let mut crated = base;
                blend_overlay_bgr(&mut crated, size, crate_icon, false);
                let crated_phash = compute_phash(&crated, size as usize, size as usize);
                templates.push(IconTemplate {
                    image: crated,
                    code: item.code.clone(),
                    mod_name: mod_name.to_string(),
                    faction: item.faction,
                    category: item.category,
                    crated: true,
                    phash: crated_phash,
                });
            }
        }

        templates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod faction_category_index_round_trip_tests {
        use super::*;

        #[test]
        fn faction_index_round_trips() {
            for faction in [
                ItemFaction::Neutral,
                ItemFaction::Colonials,
                ItemFaction::Wardens,
            ] {
                assert_eq!(ItemFaction::from_index(faction.as_index()), faction);
            }
        }

        #[test]
        fn category_index_round_trips() {
            for category in [
                ItemCategory::Item,
                ItemCategory::Vehicle,
                ItemCategory::Shippable,
                ItemCategory::Invalid,
            ] {
                assert_eq!(ItemCategory::from_index(category.as_index()), category);
            }
        }
    }

    mod normalize_icon_path_tests {
        use super::*;

        #[test]
        fn strips_numeric_version_suffix() {
            assert_eq!(
                normalize_icon_path("War/Content/Textures/Icon.0"),
                "War/Content/Textures/Icon"
            );
        }

        #[test]
        fn leaves_path_without_suffix_untouched() {
            assert_eq!(
                normalize_icon_path("War/Content/Textures/Icon"),
                "War/Content/Textures/Icon"
            );
        }

        #[test]
        fn leaves_non_numeric_suffix_untouched() {
            assert_eq!(normalize_icon_path("Icon.png"), "Icon.png");
        }
    }

    mod derive_catalog_icon_item_tests {
        use super::*;

        #[test]
        fn missing_code_name_returns_none() {
            let entry = json!({"Icon": "War/Content/Icon.0"});
            assert!(derive_catalog_icon_item(&entry).is_none());
        }

        #[test]
        fn empty_code_name_returns_none() {
            let entry = json!({"CodeName": "", "Icon": "War/Content/Icon.0"});
            assert!(derive_catalog_icon_item(&entry).is_none());
        }

        #[test]
        fn extracts_item_fields() {
            let entry = json!({
                "CodeName": "Rifle",
                "FactionVariant": "EFactionID::Wardens",
                "StockpileCategory": "Items",
                "Icon": "War/Content/Textures/T_Rifle.0",
            });
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.code, "Rifle");
            assert_eq!(item.faction, ItemFaction::Wardens);
            assert_eq!(item.category, ItemCategory::Item);
            assert_eq!(item.icon_path, "War/Content/Textures/T_Rifle");
        }

        #[test]
        fn vehicles_category_is_vehicle() {
            let entry = json!({"CodeName": "Tank", "StockpileCategory": "Vehicles"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.category, ItemCategory::Vehicle);
        }

        #[test]
        fn structures_category_is_shippable() {
            let entry = json!({"CodeName": "Bunker", "StockpileCategory": "Structures"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.category, ItemCategory::Shippable);
        }

        #[test]
        fn missing_category_is_invalid() {
            let entry = json!({"CodeName": "Unknown"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.category, ItemCategory::Invalid);
        }

        #[test]
        fn unknown_category_is_invalid() {
            let entry = json!({"CodeName": "Unknown", "StockpileCategory": "Data"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.category, ItemCategory::Invalid);
        }

        #[test]
        fn default_faction_is_neutral() {
            let entry = json!({"CodeName": "Item"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.faction, ItemFaction::Neutral);
        }

        #[test]
        fn extracts_subicon_path_and_cratable_flag() {
            let entry = json!({
                "CodeName": "AAAmmo",
                "SubTypeIcon": "War/Content/Textures/UI/ItemIcons/SubtypeSHIcon.0",
                "bIsCratable": true,
            });
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(
                item.subicon_path.as_deref(),
                Some("War/Content/Textures/UI/ItemIcons/SubtypeSHIcon")
            );
            assert!(item.cratable);
        }

        #[test]
        fn missing_subicon_and_cratable_default_to_none_and_false() {
            let entry = json!({"CodeName": "PlainItem"});
            let item = derive_catalog_icon_item(&entry).unwrap();
            assert_eq!(item.subicon_path, None);
            assert!(!item.cratable);
        }

        #[test]
        fn cratable_requires_true_flag() {
            let entry = json!({"CodeName": "Rifle", "bIsCratable": false});
            assert!(!derive_catalog_icon_item(&entry).unwrap().cratable);
            let entry = json!({"CodeName": "Rifle", "bIsCratable": "true"});
            assert!(!derive_catalog_icon_item(&entry).unwrap().cratable);
        }
    }

    mod enum_name_tests {
        use super::*;

        #[test]
        fn faction_names_are_lowercase() {
            assert_eq!(ItemFaction::Neutral.name(), "neutral");
            assert_eq!(ItemFaction::Colonials.name(), "colonials");
            assert_eq!(ItemFaction::Wardens.name(), "wardens");
        }

        #[test]
        fn category_names_are_lowercase() {
            assert_eq!(ItemCategory::Item.name(), "item");
            assert_eq!(ItemCategory::Vehicle.name(), "vehicle");
            assert_eq!(ItemCategory::Shippable.name(), "shippable");
            assert_eq!(ItemCategory::Invalid.name(), "invalid");
        }
    }

    mod resolve_resolutions_tests {
        use super::*;

        #[test]
        fn no_request_means_every_supported_resolution() {
            assert_eq!(
                resolve_resolutions(&[]).unwrap(),
                SUPPORTED_RESOLUTIONS.to_vec()
            );
        }

        #[test]
        fn requested_resolutions_are_sorted_and_deduplicated() {
            assert_eq!(
                resolve_resolutions(&[1440, 1080, 1440]).unwrap(),
                vec![1080, 1440]
            );
        }

        #[test]
        fn unsupported_resolution_is_rejected() {
            let err = resolve_resolutions(&[1080, 1234]).unwrap_err().to_string();
            assert!(err.contains("1234"), "{err}");
        }
    }

    mod icon_size_tests {
        use super::*;

        #[test]
        fn truncates_at_1080p() {
            // 64/2160 * 1080 = 32.0 exactly.
            assert_eq!(icon_size_for_resolution(1080), 32);
        }

        #[test]
        fn truncates_at_2160p() {
            assert_eq!(icon_size_for_resolution(2160), 64);
        }

        #[test]
        fn truncates_rather_than_rounds() {
            // 64/2160 * 900 = 26.666... -> truncates to 26, not 27.
            assert_eq!(icon_size_for_resolution(900), 26);
        }
    }

    mod phash_tests {
        use super::*;

        #[test]
        fn empty_image_hashes_to_zero() {
            assert_eq!(compute_phash(&[], 0, 0), 0);
        }

        #[test]
        fn uniform_images_hash_to_zero() {
            let white: Vec<u8> = vec![255; 64 * 64 * 3];
            let black: Vec<u8> = vec![0; 64 * 64 * 3];
            assert_eq!(compute_phash(&white, 64, 64), 0);
            assert_eq!(compute_phash(&black, 64, 64), 0);
        }

        #[test]
        fn gradient_produces_nonzero_hash() {
            let mut gradient = vec![0u8; 64 * 64 * 3];
            for y in 0..64 {
                for x in 0..64 {
                    let idx = (y * 64 + x) * 3;
                    let val = (x * 255 / 63) as u8;
                    gradient[idx] = val;
                    gradient[idx + 1] = val;
                    gradient[idx + 2] = val;
                }
            }
            let hash = compute_phash(&gradient, 64, 64);
            assert_ne!(hash, 0);
            assert_ne!(hash, 0xFFFF_FFFF_FFFF_FFFF);
        }

        #[test]
        fn bgr_grayscale_matches_opencv_formula() {
            let red = vec![0u8, 0, 255];
            assert_eq!(bgr_to_grayscale(&red, 1, 1)[0], 76);
            let green = vec![0u8, 255, 0];
            assert_eq!(bgr_to_grayscale(&green, 1, 1)[0], 150);
            let blue = vec![255u8, 0, 0];
            assert_eq!(bgr_to_grayscale(&blue, 1, 1)[0], 29);
        }

        #[test]
        fn matches_opencv_phash_for_fractional_and_integer_scales() {
            // Reference hashes computed with OpenCV (grayscale + INTER_AREA
            // to 8x8) on the same patterns: 19 and 45 take OpenCV's
            // fractional INTER_AREA path, 32 and 64 its integer-scale path.
            for (size, expected) in [
                (19usize, 0x076c_07de_07ff_07fe_u64),
                (32, 0x361e_3731_3574_3b6f),
                (45, 0x3a38_3a73_635f_5f5c),
                (64, 0x7650_4577_99c6_a39d),
            ] {
                let mut bgr = vec![0u8; size * size * 3];
                for y in 0..size {
                    for x in 0..size {
                        let idx = (y * size + x) * 3;
                        bgr[idx] = ((x * 37 + y * 11) % 256) as u8;
                        bgr[idx + 1] = ((x * 7 + y * 53) % 256) as u8;
                        bgr[idx + 2] = ((x * x + y * 3) % 256) as u8;
                    }
                }
                assert_eq!(compute_phash(&bgr, size, size), expected, "size {size}");
            }
        }
    }

    mod build_icon_templates_tests {
        use super::*;
        use image::{Rgba, RgbaImage};

        fn item(cratable: bool, subicon_path: Option<&str>) -> CatalogIconItem {
            CatalogIconItem {
                code: "Rifle".to_string(),
                faction: ItemFaction::Colonials,
                category: ItemCategory::Item,
                icon_path: "War/Content/T_Rifle".to_string(),
                subicon_path: subicon_path.map(String::from),
                cratable,
            }
        }

        fn opaque_image(color: [u8; 3]) -> DynamicImage {
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                64,
                64,
                Rgba([color[0], color[1], color[2], 255]),
            ))
        }

        /// `w x h` RGBA pattern shifted by `k`, with varied alpha.
        fn pattern(w: u32, h: u32, k: u32) -> DynamicImage {
            DynamicImage::ImageRgba8(RgbaImage::from_fn(w, h, |x, y| {
                Rgba([
                    ((x * x + y * 3 + k) % 256) as u8,
                    ((x * 7 + y * 53 + k) % 256) as u8,
                    ((x * 37 + y * 11 + k) % 256) as u8,
                    ((x * 29 + y * 17 + k * 3) % 256) as u8,
                ])
            }))
        }

        /// Plain and position-weighted byte sums of a BGR buffer.
        fn checksums(bgr: &[u8]) -> (u64, u64) {
            let sum = bgr.iter().map(|&b| b as u64).sum();
            let weighted = bgr
                .iter()
                .enumerate()
                .map(|(i, &b)| (i as u64 + 1) * b as u64)
                .sum();
            (sum, weighted)
        }

        #[test]
        fn composition_matches_pillow_reference_values() {
            // Reference checksums of the base icon and crate-overlay output
            // computed with Pillow (LANCZOS) and numpy float32 blending for
            // the same patterns at 32px.
            let main = pattern(40, 40, 0);
            let subicon = pattern(20, 20, 5);
            let crate_icon = pattern(24, 24, 9);

            let plain = build_icon_templates(
                &item(true, None),
                "vanilla",
                &main,
                None,
                Some(&crate_icon),
                1080,
            );
            assert_eq!(checksums(&plain[0].image), (192_874, 300_169_976));
            assert_eq!(checksums(&plain[1].image), (213_713, 350_797_520));

            let with_subicon = build_icon_templates(
                &item(true, Some("War/Content/Subicon")),
                "vanilla",
                &main,
                Some(&subicon),
                Some(&crate_icon),
                1080,
            );
            assert_eq!(checksums(&with_subicon[0].image), (214_619, 314_506_713));
            assert_eq!(checksums(&with_subicon[1].image), (235_458, 365_134_257));
        }

        #[test]
        fn non_cratable_item_produces_a_single_uncreated_template() {
            let item = item(false, None);
            let img = opaque_image([100, 150, 200]);
            let templates = build_icon_templates(&item, "vanilla", &img, None, None, 1080);

            assert_eq!(templates.len(), 1);
            assert!(!templates[0].crated);
            assert_eq!(templates[0].code, "Rifle");
            assert_eq!(templates[0].mod_name, "vanilla");
            assert_eq!(templates[0].faction, ItemFaction::Colonials);
            assert_eq!(templates[0].category, ItemCategory::Item);
            assert_eq!(templates[0].image.len(), 32 * 32 * 3);
        }

        #[test]
        fn cratable_item_with_crate_icon_produces_both_variants() {
            let item = item(true, None);
            let img = opaque_image([100, 150, 200]);
            let crate_icon = opaque_image([10, 20, 30]);
            let templates =
                build_icon_templates(&item, "vanilla", &img, None, Some(&crate_icon), 1080);

            assert_eq!(templates.len(), 2);
            assert!(!templates[0].crated);
            assert!(templates[1].crated);
            // The crate overlay changes the bottom-right corner of the image.
            assert_ne!(templates[0].image, templates[1].image);
        }

        #[test]
        fn cratable_item_without_crate_icon_available_only_produces_uncreated_template() {
            let item = item(true, None);
            let img = opaque_image([100, 150, 200]);
            let templates = build_icon_templates(&item, "vanilla", &img, None, None, 1080);

            assert_eq!(templates.len(), 1);
            assert!(!templates[0].crated);
        }

        #[test]
        fn subicon_blends_into_the_base_template() {
            let item = item(false, Some("War/Content/Subicon"));
            let img = opaque_image([100, 150, 200]);
            let subicon = opaque_image([255, 255, 255]);
            let without = build_icon_templates(&item, "vanilla", &img, None, None, 1080);
            let with = build_icon_templates(&item, "vanilla", &img, Some(&subicon), None, 1080);

            assert_ne!(without[0].image, with[0].image);
        }

        #[test]
        fn blank_subicon_is_not_blended() {
            let item = item(false, Some("War/Content/Subicon"));
            let img = opaque_image([100, 150, 200]);
            let placeholder = opaque_image([90, 90, 90]);
            let without = build_icon_templates(&item, "vanilla", &img, None, None, 1080);
            let with = build_icon_templates(&item, "vanilla", &img, Some(&placeholder), None, 1080);

            assert_eq!(without[0].image, with[0].image);
        }

        #[test]
        fn solid_dark_crate_icon_is_still_blended() {
            let item = item(true, None);
            let img = opaque_image([100, 150, 200]);
            let crate_icon = opaque_image([90, 90, 90]);
            let templates =
                build_icon_templates(&item, "vanilla", &img, None, Some(&crate_icon), 1080);

            assert_ne!(templates[0].image, templates[1].image);
        }
    }

    mod is_blank_subicon_tests {
        use super::*;
        use image::{Rgba, RgbaImage};

        fn filled(color: [u8; 4]) -> RgbaImage {
            RgbaImage::from_pixel(20, 20, Rgba(color))
        }

        #[test]
        fn fully_transparent_subicon_is_blank() {
            let image = DynamicImage::ImageRgba8(filled([255, 255, 255, 0]));
            assert!(is_blank_subicon(&image));
        }

        #[test]
        fn barely_visible_subicon_is_blank() {
            // 19 of 400 pixels visible: under the 5% threshold.
            let mut pixels = filled([255, 255, 255, 0]);
            for x in 0..19 {
                pixels.put_pixel(x, 0, Rgba([255, 255, 255, 255]));
            }
            assert!(is_blank_subicon(&DynamicImage::ImageRgba8(pixels)));
        }

        #[test]
        fn faint_alpha_does_not_count_as_visible() {
            let image = DynamicImage::ImageRgba8(filled([255, 255, 255, 10]));
            assert!(is_blank_subicon(&image));
        }

        #[test]
        fn solid_dark_gray_subicon_is_blank() {
            let image = DynamicImage::ImageRgba8(filled([90, 90, 90, 255]));
            assert!(is_blank_subicon(&image));
        }

        #[test]
        fn solid_light_subicon_is_not_blank() {
            let image = DynamicImage::ImageRgba8(filled([200, 200, 200, 255]));
            assert!(!is_blank_subicon(&image));
        }

        #[test]
        fn detailed_dark_subicon_is_not_blank() {
            let mut pixels = filled([0, 0, 0, 255]);
            for y in 0..20 {
                for x in (0..20).step_by(2) {
                    pixels.put_pixel(x, y, Rgba([200, 200, 200, 255]));
                }
            }
            assert!(!is_blank_subicon(&DynamicImage::ImageRgba8(pixels)));
        }

        #[test]
        fn empty_subicon_is_blank() {
            let image = DynamicImage::ImageRgba8(RgbaImage::new(0, 0));
            assert!(is_blank_subicon(&image));
        }
    }

    mod composite_subicon_tests {
        use super::*;
        use image::{Rgba, RgbaImage};

        fn icon(color: [u8; 4]) -> DynamicImage {
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, Rgba(color)))
        }

        #[test]
        fn draws_the_subicon_only_over_the_top_left_corner() {
            let base = icon([0, 0, 0, 255]);
            let composited = composite_subicon(&base, &icon([255, 255, 255, 255]));

            // 32 * 7/16 = 14px overlay.
            assert_eq!(composited.dimensions(), (32, 32));
            assert_ne!(composited.get_pixel(0, 0), &Rgba([0, 0, 0, 255]));
            assert_eq!(composited.get_pixel(0, 0)[3], 255);
            assert_eq!(composited.get_pixel(14, 14), &Rgba([0, 0, 0, 255]));
            assert_eq!(composited.get_pixel(31, 0), &Rgba([0, 0, 0, 255]));
        }

        #[test]
        fn keeps_the_icon_transparent_outside_the_subicon() {
            let composited = composite_subicon(&icon([0, 0, 0, 0]), &icon([255, 255, 255, 255]));

            // The subicon is drawn at 75% opacity over a transparent icon.
            assert_eq!(composited.get_pixel(0, 0)[3], 191);
            assert_eq!(composited.get_pixel(20, 20), &Rgba([0, 0, 0, 0]));
        }

        #[test]
        fn blank_subicon_leaves_the_icon_untouched() {
            let base = icon([10, 20, 30, 255]);
            let composited = composite_subicon(&base, &icon([90, 90, 90, 255]));

            assert_eq!(composited, base.to_rgba8());
        }
    }
}
