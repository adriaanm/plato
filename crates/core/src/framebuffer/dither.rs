//! Error diffusion to G16, in linear light.
//!
//! Plato already dithers with an ordered blue-noise mask (see `transform.rs`):
//! each pixel is nudged by a precomputed drift and rounded to the nearest of
//! the sixteen gray levels the panel can show, G16 := {17 * i | i ∈ {0 .. 15}}.
//! A mask is stateless — every pixel is judged alone — which is exactly what
//! you want for UI and whole-screen work: it's cheap, it composes with partial
//! refreshes, and flat fills come out clean. But a stateless dither also
//! *forgets*: the rounding error of one pixel is thrown away, and in a smooth
//! photographic gradient those forgotten errors line up into visible banding
//! and mottle. Error diffusion keeps the books instead — each pixel's
//! quantization debt is carried to its unvisited neighbors and repaid there —
//! so the local mean of the output tracks the local mean of the input, which
//! is precisely the property a smooth gradient needs when only 16 levels
//! exist. That makes diffusion the right tool for an article's images, applied
//! once after scaling, and the mask the right tool for everything else.
//!
//! Why linear light: the 8-bit values in a pixmap are sRGB *code* values, a
//! perceptually spaced scale, not an energy scale. Diffusion's promise — the
//! neighborhood mean is preserved — only means anything physical if the
//! averaging happens in the space where light adds, i.e. linear luminance.
//! Quantize and diffuse in code space and you average the wrong quantity; the
//! classic symptom is midtones that dry out (a 50% code checkerboard of 0 and
//! 255 reflects far more light than a flat 128 field). So we convert through
//! the piecewise sRGB EOTF — c/12.92 below the 0.04045 knee, otherwise
//! ((c + 0.055)/1.055)^2.4 (IEC 61966-2-1) — diffuse the error there, and pick
//! the nearest G16 level by *linear* distance. The sixteen linear values are
//! not uniformly spaced (the gap below white is ~180x the gap above black), so
//! nearest-by-code and nearest-by-linear disagree; we precompute the fifteen
//! linear midpoints and binary-search them.
//!
//! Why serpentine: Floyd–Steinberg pushes 7/16 of every error toward the same
//! side when all rows scan left-to-right, and the correlated residue drags
//! visible "worms" diagonally across flat regions. Alternating the scan
//! direction each row (and mirroring the kernel) cancels the directional bias.
//! Atkinson gets the serpentine treatment too, and for the same reason: its
//! kernel reaches two pixels ahead, so a fixed direction biases even harder;
//! mirroring costs nothing in the shared traversal.
//!
//! Why Stucki is the one the engine calls: even serpentine, Floyd–Steinberg's
//! four-neighbor kernel lets the dots organize — on a smooth ramp they line up
//! into faint columns (see `examples/dither_ab.rs`, which is where to look
//! whenever this choice is questioned). Stucki spreads the same fully-repaid
//! error over twelve neighbors and three rows, thin enough that no structure
//! survives, and the traversal is memory-bound enough that the tripled kernel
//! costs ~13% — the cheapest visible quality step in this file.
//!
//! Why offer Atkinson at all: Bill Atkinson's kernel deliberately diffuses
//! only 6/8 of the error and drops the rest. Losing a quarter of the debt
//! means the mean is *not* exactly preserved — near-black and near-white
//! detail washes toward the extremes — but it also means errors die out
//! quickly instead of snaking through flat areas, which reads as the crisp,
//! bright look of classic Macintosh dithering. On a reflective e-ink panel,
//! whose "white" is grayish paper, that extra snap in the highlights is often
//! the better trade for photographs.

use lazy_static::lazy_static;
use super::image::Pixmap;

/// Number of representable gray levels.
const LEVELS: usize = 16;
/// Code-value gap between two successive G16 levels.
const STEP: u8 = 17;

lazy_static! {
    // sRGB code value → linear luminance in [0, 1], via the piecewise
    // sRGB EOTF (IEC 61966-2-1): c/12.92 below the 0.04045 knee,
    // ((c + 0.055)/1.055)^2.4 above it.
    static ref SRGB_TO_LINEAR: [f32; 256] = {
        let mut lut = [0.0f32; 256];
        for (v, l) in lut.iter_mut().enumerate() {
            let c = v as f64 / 255.0;
            *l = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            } as f32;
        }
        lut
    };

    // Linear luminance of each G16 level.
    static ref G16_LINEAR: [f32; LEVELS] = {
        let mut lut = [0.0f32; LEVELS];
        for (i, l) in lut.iter_mut().enumerate() {
            *l = SRGB_TO_LINEAR[i * STEP as usize];
        }
        lut
    };

    // Midpoints between successive G16 linear values: the fifteen decision
    // thresholds for nearest-by-linear-distance quantization.
    static ref G16_THRESHOLDS: [f32; LEVELS - 1] = {
        let mut lut = [0.0f32; LEVELS - 1];
        for (i, t) in lut.iter_mut().enumerate() {
            *t = 0.5 * (G16_LINEAR[i] + G16_LINEAR[i + 1]);
        }
        lut
    };
}

// A diffusion kernel entry: (dx, dy, weight), with dx expressed in the
// traversal direction (mirrored on reverse rows) and dy ≥ 0.
type KernelEntry = (isize, usize, f32);

// Floyd–Steinberg: the classic 7/16, 3/16, 5/16, 1/16 — all of the error is
// repaid, so the neighborhood mean is preserved exactly.
const FLOYD_STEINBERG: [KernelEntry; 4] = [
    (1, 0, 7.0 / 16.0),
    (-1, 1, 3.0 / 16.0),
    (0, 1, 5.0 / 16.0),
    (1, 1, 1.0 / 16.0),
];

// Stucki: twelve neighbors over three rows, everything repaid (÷42). The wide
// reach spreads each pixel's debt thin, which is what dissolves the faint
// worm texture Floyd–Steinberg can leave in large flat regions -- at three
// times the kernel work per pixel.
const STUCKI: [KernelEntry; 12] = [
    (1, 0, 8.0 / 42.0),
    (2, 0, 4.0 / 42.0),
    (-2, 1, 2.0 / 42.0),
    (-1, 1, 4.0 / 42.0),
    (0, 1, 8.0 / 42.0),
    (1, 1, 4.0 / 42.0),
    (2, 1, 2.0 / 42.0),
    (-2, 2, 1.0 / 42.0),
    (-1, 2, 2.0 / 42.0),
    (0, 2, 4.0 / 42.0),
    (1, 2, 2.0 / 42.0),
    (2, 2, 1.0 / 42.0),
];

// Atkinson: 1/8 to each of six neighbors; the remaining quarter of the error
// is dropped on purpose (see the module comment).
const ATKINSON: [KernelEntry; 6] = [
    (1, 0, 1.0 / 8.0),
    (2, 0, 1.0 / 8.0),
    (-1, 1, 1.0 / 8.0),
    (0, 1, 1.0 / 8.0),
    (1, 1, 1.0 / 8.0),
    (0, 2, 1.0 / 8.0),
];

/// Floyd–Steinberg error diffusion to G16, in linear light, serpentine.
///
/// Operates in place on an 8-bit grayscale pixmap (`samples == 1`); pixmaps
/// with any other layout are left untouched. Meant to be applied once to an
/// image's pixmap after scaling, not per blit.
pub fn dither_g16_floyd_steinberg(pixmap: &mut Pixmap) {
    if pixmap.samples != 1 || pixmap.data.is_empty() {
        return;
    }
    let (width, height) = (pixmap.width as usize, pixmap.height as usize);
    error_diffuse(pixmap.data_mut(), width, height, &FLOYD_STEINBERG);
}

/// Stucki error diffusion to G16, in linear light, serpentine.
///
/// Same contract as [`dither_g16_floyd_steinberg`], and like it repays every
/// bit of the error -- just over twelve neighbors instead of four, which
/// trades kernel work for a smoother, less directional grain.
pub fn dither_g16_stucki(pixmap: &mut Pixmap) {
    if pixmap.samples != 1 || pixmap.data.is_empty() {
        return;
    }
    let (width, height) = (pixmap.width as usize, pixmap.height as usize);
    error_diffuse(pixmap.data_mut(), width, height, &STUCKI);
}

/// Atkinson error diffusion to G16, in linear light, serpentine.
///
/// Same contract as [`dither_g16_floyd_steinberg`]. Diffuses only 6/8 of each
/// pixel's error, trading exact mean preservation near the extremes for a
/// crisper, brighter rendition.
pub fn dither_g16_atkinson(pixmap: &mut Pixmap) {
    if pixmap.samples != 1 || pixmap.data.is_empty() {
        return;
    }
    let (width, height) = (pixmap.width as usize, pixmap.height as usize);
    error_diffuse(pixmap.data_mut(), width, height, &ATKINSON);
}

// Shared traversal: serpentine scan, error carried in `1 + max(dy)` rolling
// rows of f32 rather than a full-image buffer. The accumulated value is
// clamped to [0, 1] *before* quantization, which bounds the error a pixel can
// emit by half the widest linear gap (~0.078, just below white); a hard edge
// therefore injects a bounded debt that the kernel amortizes within a few
// pixels instead of ringing forever.
fn error_diffuse(data: &mut [u8], width: usize, height: usize, kernel: &[KernelEntry]) {
    if width == 0 || height == 0 {
        return;
    }
    let srgb_to_linear = &*SRGB_TO_LINEAR;
    let g16_linear = &*G16_LINEAR;
    let g16_thresholds = &*G16_THRESHOLDS;
    let max_dy = kernel.iter().map(|&(_, dy, _)| dy).max().unwrap_or(0);
    // rows[0] is the current row's incoming error; rows[dy] the row dy below.
    let mut rows: Vec<Vec<f32>> = vec![vec![0.0f32; width]; max_dy + 1];
    for y in 0..height {
        let reverse = y % 2 == 1;
        for i in 0..width {
            let x = if reverse { width - 1 - i } else { i };
            let addr = y * width + x;
            let target = (srgb_to_linear[data[addr] as usize] + rows[0][x]).clamp(0.0, 1.0);
            let index = g16_thresholds.partition_point(|&t| t < target);
            data[addr] = index as u8 * STEP;
            let err = target - g16_linear[index];
            for &(dx, dy, weight) in kernel {
                let dx = if reverse { -dx } else { dx };
                let nx = x as isize + dx;
                if nx >= 0 && (nx as usize) < width {
                    rows[dy][nx as usize] += err * weight;
                }
            }
        }
        // Retire the current row: it becomes the farthest-ahead row, zeroed.
        rows.rotate_left(1);
        if let Some(last) = rows.last_mut() {
            last.iter_mut().for_each(|e| *e = 0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_pixmap(width: u32, height: u32, mut fill: impl FnMut(u32, u32) -> u8) -> Pixmap {
        let mut pixmap = Pixmap::new(width, height, 1);
        for y in 0..height {
            for x in 0..width {
                pixmap.data[(y * width + x) as usize] = fill(x, y);
            }
        }
        pixmap
    }

    fn mean_linear(data: &[u8]) -> f64 {
        data.iter().map(|&v| SRGB_TO_LINEAR[v as usize] as f64).sum::<f64>() / data.len() as f64
    }

    // Deterministic pseudo-random bytes: xorshift32, no rand crate.
    fn xorshift_pixmap(width: u32, height: u32, mut seed: u32) -> Pixmap {
        gray_pixmap(width, height, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 24) as u8
        })
    }

    #[test]
    fn every_output_byte_lands_in_g16_for_both_algorithms() {
        for dither in [dither_g16_floyd_steinberg, dither_g16_stucki, dither_g16_atkinson] {
            let mut pixmap = xorshift_pixmap(97, 61, 0x2545_f491);
            dither(&mut pixmap);
            assert!(pixmap.data.iter().all(|&v| v % STEP == 0),
                    "found a byte outside G16");
        }
    }

    #[test]
    fn full_repayment_kernels_preserve_the_linear_mean_of_a_mid_gray_field() {
        // 128 is not a G16 level, and its two candidates (119 and 136) are
        // both far away, so the dither must interleave them. Only diffusion
        // in linear light keeps the *linear* mean of that mixture on target;
        // diffusing code values would balance the code mean instead and land
        // measurably off in luminance. Floyd–Steinberg and Stucki both repay
        // every bit of the error, so both owe this property; Atkinson is
        // exempt by design and measured separately below.
        for dither in [dither_g16_floyd_steinberg, dither_g16_stucki] {
            let mut pixmap = gray_pixmap(128, 128, |_, _| 128);
            let input_mean = mean_linear(&pixmap.data);
            dither(&mut pixmap);
            let output_mean = mean_linear(&pixmap.data);
            let relative_error = (output_mean - input_mean).abs() / input_mean;
            assert!(relative_error < 0.01,
                    "linear mean drifted by {:.3}% (in {:.5}, out {:.5})",
                    100.0 * relative_error, input_mean, output_mean);
        }
    }

    #[test]
    fn a_gradient_tracks_its_input_column_by_column_and_keeps_its_extremes() {
        for dither in [dither_g16_floyd_steinberg, dither_g16_stucki, dither_g16_atkinson] {
            let (width, height) = (256u32, 64u32);
            let mut pixmap = gray_pixmap(width, height, |x, _| x as u8);
            dither(&mut pixmap);
            // The extremes are exact G16 levels with zero incoming energy to
            // spare: 0 must stay 0 and 255 must stay 255.
            for y in 0..height as usize {
                assert_eq!(pixmap.data[y * width as usize], 0, "black edge eroded");
                assert_eq!(pixmap.data[y * width as usize + width as usize - 1], 255,
                           "white edge eroded");
            }
            // No column may sit more than one local level gap away from its
            // own input's linear mean: diffusion smears error to neighbors,
            // it must not displace a whole column's trend.
            for x in 0..width as usize {
                let column: Vec<u8> = (0..height as usize)
                    .map(|y| pixmap.data[y * width as usize + x])
                    .collect();
                let column_mean = mean_linear(&column);
                let input_linear = SRGB_TO_LINEAR[x] as f64;
                let level = x / STEP as usize;
                let gap = G16_LINEAR[(level + 1).min(LEVELS - 1)] as f64
                    - G16_LINEAR[level.saturating_sub(1)] as f64;
                assert!((column_mean - input_linear).abs() <= gap,
                        "column {} drifted {:.5} (allowed {:.5})",
                        x, (column_mean - input_linear).abs(), gap);
            }
        }
    }

    #[test]
    fn atkinson_loses_error_by_design_but_stays_within_half_a_level() {
        // 105 sits close to the G16 level 102 and far from the linear
        // midpoint toward 119, so nearly every pixel rounds down and the
        // occasional 119 repays the debt. Floyd–Steinberg repays it in full;
        // Atkinson drops a quarter of it each step, so its mean must drift
        // farther — that loss is the feature, not a bug — while still staying
        // inside the half-gap that separates 105 from ever misrounding.
        // (128 would be a poor probe: it sits almost exactly on a linear
        // midpoint, where both algorithms drift by nearly nothing.)
        let mut fs = gray_pixmap(128, 128, |_, _| 105);
        let mut atkinson = fs.clone();
        let input_mean = mean_linear(&fs.data);
        dither_g16_floyd_steinberg(&mut fs);
        dither_g16_atkinson(&mut atkinson);
        let fs_drift = (mean_linear(&fs.data) - input_mean).abs();
        let atkinson_drift = (mean_linear(&atkinson.data) - input_mean).abs();
        assert!(atkinson_drift > fs_drift,
                "atkinson drift {:.6} not larger than floyd–steinberg's {:.6}",
                atkinson_drift, fs_drift);
        let half_gap = 0.5 * (G16_LINEAR[7] - G16_LINEAR[6]) as f64;
        assert!(atkinson_drift <= half_gap,
                "atkinson drift {:.6} exceeds half the local gap {:.6}",
                atkinson_drift, half_gap);
    }

    #[test]
    fn degenerate_shapes_do_not_panic() {
        for dither in [dither_g16_floyd_steinberg, dither_g16_stucki, dither_g16_atkinson] {
            let mut single = gray_pixmap(1, 1, |_, _| 200);
            dither(&mut single);
            assert!(single.data[0] % STEP == 0);

            let mut row = gray_pixmap(37, 1, |x, _| (x * 7) as u8);
            dither(&mut row);
            assert!(row.data.iter().all(|&v| v % STEP == 0));

            let mut column = gray_pixmap(1, 37, |_, y| (y * 7) as u8);
            dither(&mut column);
            assert!(column.data.iter().all(|&v| v % STEP == 0));

            let mut empty = Pixmap::empty(5, 5, 1);
            dither(&mut empty);
        }
    }

    #[test]
    fn pure_black_and_pure_white_pass_through_unchanged() {
        for dither in [dither_g16_floyd_steinberg, dither_g16_stucki, dither_g16_atkinson] {
            for value in [0u8, 255u8] {
                let mut pixmap = gray_pixmap(64, 48, |_, _| value);
                dither(&mut pixmap);
                assert!(pixmap.data.iter().all(|&v| v == value),
                        "a flat {} field did not survive", value);
            }
        }
    }

    #[test]
    fn color_pixmaps_are_left_untouched() {
        let mut pixmap = Pixmap::new(8, 8, 3);
        pixmap.data.iter_mut().enumerate().for_each(|(i, v)| *v = (i * 11) as u8);
        let before = pixmap.data.clone();
        dither_g16_floyd_steinberg(&mut pixmap);
        dither_g16_atkinson(&mut pixmap);
        assert_eq!(pixmap.data, before);
    }

    #[test]
    fn rough_throughput_on_a_megapixel() {
        // Not an assertion, a measurement: the target device turns pages on
        // a 1 GHz Cortex-A9 and dithers about one megapixel per article page.
        let mut pixmap = xorshift_pixmap(1024, 1024, 0xdead_beef);
        let start = std::time::Instant::now();
        dither_g16_floyd_steinberg(&mut pixmap);
        let fs = start.elapsed();
        let mut pixmap = xorshift_pixmap(1024, 1024, 0xdead_beef);
        let start = std::time::Instant::now();
        dither_g16_stucki(&mut pixmap);
        let stucki = start.elapsed();
        let mut pixmap = xorshift_pixmap(1024, 1024, 0xdead_beef);
        let start = std::time::Instant::now();
        dither_g16_atkinson(&mut pixmap);
        let atkinson = start.elapsed();
        let pixels = 1024.0 * 1024.0;
        println!("floyd–steinberg: {:.1} ns/pixel, stucki: {:.1} ns/pixel, \
                  atkinson: {:.1} ns/pixel",
                 fs.as_nanos() as f64 / pixels,
                 stucki.as_nanos() as f64 / pixels,
                 atkinson.as_nanos() as f64 / pixels);
    }
}
