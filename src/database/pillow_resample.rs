//! Pillow-compatible `Image.resize(..., Resampling.LANCZOS)` for RGBA images.
//!
//! Every icon and overlay is resized with Pillow's LANCZOS filter semantics.
//! The `image` crate's `Lanczos3` uses floating-point weights and a single
//! clamp, so its output drifts from Pillow's by a few levels per pixel. This
//! module reproduces Pillow's exact arithmetic so the templates written by
//! `add-mod` match Pillow's output byte for byte:
//!
//! 1. RGBA is premultiplied (`RGBA -> RGBa`) with Pillow's rounded `x * a / 255`.
//! 2. Separable two-pass convolution (horizontal, then vertical) with a
//!    3-lobe Lanczos kernel whose weights are normalized per output pixel and
//!    quantized to 22-bit fixed point.
//! 3. The result is un-premultiplied (`RGBa -> RGBA`) with integer division.

use std::f64::consts::PI;

use image::RgbaImage;

/// Fixed-point precision of the quantized kernel weights.
const PRECISION_BITS: u32 = 32 - 8 - 2;
/// Half-width of the Lanczos kernel in source pixels at scale 1.
const LANCZOS_SUPPORT: f64 = 3.0;

/// Kernel weights for one output pixel along one axis.
struct AxisWindow {
    /// First source index the window reads.
    start: usize,
    /// Quantized weights, one per source index from `start`.
    weights: Vec<i64>,
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * PI;
    x.sin() / x
}

fn lanczos(x: f64) -> f64 {
    if (-LANCZOS_SUPPORT..LANCZOS_SUPPORT).contains(&x) {
        sinc(x) * sinc(x / LANCZOS_SUPPORT)
    } else {
        0.0
    }
}

/// Precomputes the per-output-pixel windows for resizing `in_size` to
/// `out_size` along one axis, with Pillow's rounding of window bounds and
/// fixed-point quantization of the normalized weights.
fn axis_windows(in_size: usize, out_size: usize) -> Vec<AxisWindow> {
    let scale = in_size as f64 / out_size as f64;
    let filter_scale = scale.max(1.0);
    let support = LANCZOS_SUPPORT * filter_scale;
    let one = (1i64 << PRECISION_BITS) as f64;

    (0..out_size)
        .map(|out| {
            let center = (out as f64 + 0.5) * scale;
            let start = ((center - support + 0.5) as i64).max(0) as usize;
            let end = ((center + support + 0.5) as i64).min(in_size as i64) as usize;
            let raw: Vec<f64> = (start..end)
                .map(|src| lanczos((src as f64 - center + 0.5) / filter_scale))
                .collect();
            let total: f64 = raw.iter().sum();
            let weights = raw
                .iter()
                .map(|&w| {
                    let w = if total != 0.0 { w / total } else { w };
                    if w < 0.0 {
                        (-0.5 + w * one) as i64
                    } else {
                        (0.5 + w * one) as i64
                    }
                })
                .collect();
            AxisWindow { start, weights }
        })
        .collect()
}

/// Rounds a fixed-point accumulator back to an 8-bit channel, saturating.
fn clip8(acc: i64) -> u8 {
    if acc >= (1i64 << PRECISION_BITS << 8) {
        255
    } else if acc <= 0 {
        0
    } else {
        (acc >> PRECISION_BITS) as u8
    }
}

/// Pillow's rounded `a * b / 255`.
fn mul_div_255(a: u8, b: u8) -> u8 {
    let tmp = a as u32 * b as u32 + 128;
    (((tmp >> 8) + tmp) >> 8) as u8
}

/// Resamples a premultiplied RGBA buffer along the x axis.
fn resample_horizontal(src: &[u8], width: usize, height: usize, out_width: usize) -> Vec<u8> {
    let windows = axis_windows(width, out_width);
    let mut out = vec![0u8; out_width * height * 4];
    for y in 0..height {
        for (x, window) in windows.iter().enumerate() {
            for c in 0..4 {
                let acc = window
                    .weights
                    .iter()
                    .enumerate()
                    .fold(1i64 << (PRECISION_BITS - 1), |acc, (i, &w)| {
                        acc + src[(y * width + window.start + i) * 4 + c] as i64 * w
                    });
                out[(y * out_width + x) * 4 + c] = clip8(acc);
            }
        }
    }
    out
}

/// Resamples a premultiplied RGBA buffer along the y axis.
fn resample_vertical(src: &[u8], width: usize, height: usize, out_height: usize) -> Vec<u8> {
    let windows = axis_windows(height, out_height);
    let mut out = vec![0u8; width * out_height * 4];
    for (y, window) in windows.iter().enumerate() {
        for x in 0..width {
            for c in 0..4 {
                let acc = window
                    .weights
                    .iter()
                    .enumerate()
                    .fold(1i64 << (PRECISION_BITS - 1), |acc, (i, &w)| {
                        acc + src[((window.start + i) * width + x) * 4 + c] as i64 * w
                    });
                out[(y * width + x) * 4 + c] = clip8(acc);
            }
        }
    }
    out
}

/// Resizes `image` to `width x height` exactly like Pillow's
/// `Image.resize((width, height), Resampling.LANCZOS)` on an RGBA image.
pub fn resize_lanczos_rgba(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    if image.dimensions() == (width, height) {
        return image.clone();
    }
    let (in_w, in_h) = (image.width() as usize, image.height() as usize);
    let (out_w, out_h) = (width as usize, height as usize);

    let mut buffer: Vec<u8> = image
        .pixels()
        .flat_map(|px| {
            [
                mul_div_255(px[0], px[3]),
                mul_div_255(px[1], px[3]),
                mul_div_255(px[2], px[3]),
                px[3],
            ]
        })
        .collect();
    let mut current_w = in_w;
    if in_w != out_w {
        buffer = resample_horizontal(&buffer, in_w, in_h, out_w);
        current_w = out_w;
    }
    if in_h != out_h {
        buffer = resample_vertical(&buffer, current_w, in_h, out_h);
    }

    for px in buffer.as_chunks_mut::<4>().0 {
        let alpha = px[3] as u32;
        if alpha != 0 && alpha != 255 {
            for channel in &mut px[..3] {
                *channel = ((255 * *channel as u32) / alpha).min(255) as u8;
            }
        }
    }
    RgbaImage::from_raw(width, height, buffer).expect("buffer sized width * height * 4")
}

/// Composites `image` onto a black background and returns row-major BGR
/// bytes, like Pillow's `Image.new("RGB", ..., (0, 0, 0)).paste(image,
/// mask=alpha)` followed by an RGB -> BGR swap.
pub fn flatten_on_black_to_bgr(image: &RgbaImage) -> Vec<u8> {
    image
        .pixels()
        .flat_map(|px| {
            [
                mul_div_255(px[2], px[3]),
                mul_div_255(px[1], px[3]),
                mul_div_255(px[0], px[3]),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// 13x11 RGBA pattern with every alpha level class (0, partial, 255-ish).
    fn pattern() -> RgbaImage {
        RgbaImage::from_fn(13, 11, |x, y| {
            Rgba([
                ((x * 37 + y * 11) % 256) as u8,
                ((x * 7 + y * 53) % 256) as u8,
                ((x * x + y * 3) % 256) as u8,
                ((x * 29 + y * 17) % 256) as u8,
            ])
        })
    }

    /// Pillow 12.2 `Image.fromarray(pattern).resize((5, 4), LANCZOS)`.
    const PILLOW_DOWNSCALE_5X4: [u8; 80] = [
        26, 93, 6, 38, 165, 89, 11, 108, 144, 102, 41, 213, 24, 110, 66, 104, 208, 181, 146, 66,
        56, 125, 9, 81, 179, 156, 24, 169, 158, 194, 44, 177, 45, 113, 91, 56, 214, 112, 136, 127,
        100, 109, 18, 135, 201, 113, 31, 211, 168, 111, 49, 62, 140, 159, 98, 91, 185, 160, 143,
        186, 141, 149, 29, 191, 197, 197, 33, 128, 26, 89, 67, 57, 179, 103, 101, 158, 133, 126,
        150, 195,
    ];

    /// Pillow 12.2 black background `paste(resized, mask=alpha)`, as RGB.
    const PILLOW_FLATTENED_5X4_RGB: [u8; 60] = [
        4, 14, 1, 70, 38, 5, 120, 85, 34, 10, 45, 27, 54, 47, 38, 18, 40, 3, 119, 103, 16, 110,
        135, 31, 10, 25, 20, 107, 56, 68, 53, 58, 10, 166, 94, 26, 41, 27, 12, 50, 57, 35, 135,
        117, 104, 106, 112, 22, 99, 99, 17, 6, 20, 15, 111, 64, 63, 102, 96, 115,
    ];

    #[test]
    fn downscale_matches_pillow_byte_for_byte() {
        let resized = resize_lanczos_rgba(&pattern(), 5, 4);
        assert_eq!(resized.as_raw().as_slice(), &PILLOW_DOWNSCALE_5X4);
    }

    #[test]
    fn upscale_matches_pillow_checksums() {
        let resized = resize_lanczos_rgba(&pattern(), 17, 15);
        let raw = resized.as_raw();
        let sum: u64 = raw.iter().map(|&b| b as u64).sum();
        let weighted: u64 = raw
            .iter()
            .enumerate()
            .map(|(i, &b)| (i as u64 + 1) * b as u64)
            .sum();
        assert_eq!((sum, weighted), (111_047, 59_622_260));
    }

    #[test]
    fn same_size_is_a_copy() {
        let image = pattern();
        assert_eq!(resize_lanczos_rgba(&image, 13, 11), image);
    }

    #[test]
    fn flatten_matches_pillow_paste_on_black() {
        let bgr = flatten_on_black_to_bgr(&resize_lanczos_rgba(&pattern(), 5, 4));
        let expected: Vec<u8> = PILLOW_FLATTENED_5X4_RGB
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [b, g, r])
            .collect();
        assert_eq!(bgr, expected);
    }
}
