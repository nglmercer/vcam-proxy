//! CPU color conversion, BT.601 studio-swing, 8.8 fixed-point coefficients.
//!
//! The kernels process pixels in independent pairs/blocks with no branches in
//! the hot path, which lets LLVM auto-vectorize them (SSE2 baseline on
//! x86_64). They are drop-in replaceable by `std::simd` or a GPU stage
//! (e.g. wgpu compute) without touching the pipeline plumbing.

#[inline(always)]
fn pack_px(dst: &mut [u8], at: usize, y: i32, u: i32, v: i32) {
    // BT.601: C = Y - 16, D = U - 128, E = V - 128
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    dst[at] = r.clamp(0, 255) as u8;
    dst[at + 1] = g.clamp(0, 255) as u8;
    dst[at + 2] = b.clamp(0, 255) as u8;
}

/// Packed YUY2 (`Y0 U0 Y1 V0`) 4:2:2 -> interleaved RGB24.
///
/// Returns `false` when buffer sizes are inconsistent with the geometry.
pub fn yuy2_to_rgb24(src: &[u8], dst: &mut [u8], width: u32, height: u32) -> bool {
    let npix = width as usize * height as usize;
    if src.len() < npix * 2 || dst.len() < npix * 3 {
        return false;
    }
    let pairs = npix / 2;
    for i in 0..pairs {
        let s = i * 4;
        let (y0, u, y1, v) = (
            src[s] as i32,
            src[s + 1] as i32,
            src[s + 2] as i32,
            src[s + 3] as i32,
        );
        pack_px(dst, i * 6, y0, u, v);
        pack_px(dst, i * 6 + 3, y1, u, v);
    }
    // Odd pixel count tail (width*height odd): convert the final lone Y with
    // neutral chroma rather than dropping it.
    if npix % 2 != 0 {
        let last = npix - 1;
        pack_px(dst, last * 3, src[last * 2] as i32, 128, 128);
    }
    true
}

/// Semi-planar NV12 (Y plane + interleaved UV plane) -> RGB24.
pub fn nv12_to_rgb24(src: &[u8], dst: &mut [u8], width: u32, height: u32) -> bool {
    let (w, h) = (width as usize, height as usize);
    if src.len() < w * h * 3 / 2 || dst.len() < w * h * 3 {
        return false;
    }
    let (y_plane, uv_plane) = src.split_at(w * h);
    for row in 0..h {
        let uv_row = (row / 2) * w;
        for col in 0..w {
            let y = y_plane[row * w + col] as i32;
            let uv = uv_row + (col & !1);
            let u = uv_plane[uv] as i32;
            let v = uv_plane[uv + 1] as i32;
            pack_px(dst, (row * w + col) * 3, y, u, v);
        }
    }
    true
}

/// Interleaved RGB24 -> semi-planar NV12 (Y plane + interleaved UV plane).
///
/// NV12 is the format browsers (Chrome/Firefox WebRTC) reliably accept from a
/// V4L2 capture device, and it is half the size of RGB24. Chroma is 4:2:0
/// subsampled by averaging each 2x2 RGB block. BT.601 studio swing to match
/// [`pack_px`]'s inverse coefficients.
///
/// Single-pass over 2×2 blocks (Y + UV together) for better cache locality.
/// Returns `false` when buffer sizes are inconsistent with the geometry.
pub fn rgb24_to_nv12(src: &[u8], dst: &mut [u8], width: u32, height: u32) -> bool {
    let (w, h) = (width as usize, height as usize);
    // NV12 requires even dimensions for the 4:2:0 chroma grid.
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return false;
    }
    if src.len() < w * h * 3 || dst.len() < w * h * 3 / 2 {
        return false;
    }

    let (y_plane, uv_plane) = dst.split_at_mut(w * h);

    // One pass: write four luma samples and one UV pair per 2×2 block.
    for by in (0..h).step_by(2) {
        let y_row0 = by * w;
        let y_row1 = y_row0 + w;
        let src_row0 = by * w * 3;
        let src_row1 = src_row0 + w * 3;
        let uv_row = (by / 2) * w;

        for bx in (0..w).step_by(2) {
            let s00 = src_row0 + bx * 3;
            let s01 = s00 + 3;
            let s10 = src_row1 + bx * 3;
            let s11 = s10 + 3;

            let (r00, g00, b00) = (src[s00] as i32, src[s00 + 1] as i32, src[s00 + 2] as i32);
            let (r01, g01, b01) = (src[s01] as i32, src[s01 + 1] as i32, src[s01 + 2] as i32);
            let (r10, g10, b10) = (src[s10] as i32, src[s10 + 1] as i32, src[s10 + 2] as i32);
            let (r11, g11, b11) = (src[s11] as i32, src[s11 + 1] as i32, src[s11 + 2] as i32);

            // BT.601: Y = 16 + (66R + 129G + 25B) / 256
            y_plane[y_row0 + bx] =
                (16 + ((66 * r00 + 129 * g00 + 25 * b00 + 128) >> 8)).clamp(0, 255) as u8;
            y_plane[y_row0 + bx + 1] =
                (16 + ((66 * r01 + 129 * g01 + 25 * b01 + 128) >> 8)).clamp(0, 255) as u8;
            y_plane[y_row1 + bx] =
                (16 + ((66 * r10 + 129 * g10 + 25 * b10 + 128) >> 8)).clamp(0, 255) as u8;
            y_plane[y_row1 + bx + 1] =
                (16 + ((66 * r11 + 129 * g11 + 25 * b11 + 128) >> 8)).clamp(0, 255) as u8;

            let r = (r00 + r01 + r10 + r11) / 4;
            let g = (g00 + g01 + g10 + g11) / 4;
            let b = (b00 + b01 + b10 + b11) / 4;
            // BT.601: U = 128 + (-38R - 74G + 112B) / 256
            //         V = 128 + (112R - 94G - 18B) / 256
            let u = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
            let v = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);
            let uv = uv_row + bx;
            uv_plane[uv] = u.clamp(0, 255) as u8;
            uv_plane[uv + 1] = v.clamp(0, 255) as u8;
        }
    }
    true
}

/// Interleaved RGB24 -> packed YUY2 (`Y0 U0 Y1 V0`).
///
/// Used when the user forces `--format yuy2` but the camera only delivers
/// MJPEG/RGB. Even dimensions required (2-pixel YUY2 pairs).
pub fn rgb24_to_yuy2(src: &[u8], dst: &mut [u8], width: u32, height: u32) -> bool {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 {
        return false;
    }
    if src.len() < w * h * 3 || dst.len() < w * h * 2 {
        return false;
    }

    for row in 0..h {
        for col in (0..w).step_by(2) {
            let s0 = (row * w + col) * 3;
            let s1 = s0 + 3;
            let (r0, g0, b0) = (src[s0] as i32, src[s0 + 1] as i32, src[s0 + 2] as i32);
            let (r1, g1, b1) = (src[s1] as i32, src[s1 + 1] as i32, src[s1 + 2] as i32);

            let y0 = 16 + ((66 * r0 + 129 * g0 + 25 * b0 + 128) >> 8);
            let y1 = 16 + ((66 * r1 + 129 * g1 + 25 * b1 + 128) >> 8);
            let r = (r0 + r1) / 2;
            let g = (g0 + g1) / 2;
            let b = (b0 + b1) / 2;
            let u = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
            let v = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);

            let d = (row * w + col) * 2;
            dst[d] = y0.clamp(0, 255) as u8;
            dst[d + 1] = u.clamp(0, 255) as u8;
            dst[d + 2] = y1.clamp(0, 255) as u8;
            dst[d + 3] = v.clamp(0, 255) as u8;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuy2_black_white() {
        // BT.601 reference black (Y=16) and white (Y=235), neutral chroma.
        let src = [16, 128, 235, 128];
        let mut dst = [0u8; 6];
        assert!(yuy2_to_rgb24(&src, &mut dst, 2, 1));
        assert_eq!(&dst[0..3], &[0, 0, 0]);
        assert_eq!(&dst[3..6], &[255, 255, 255]);
    }

    #[test]
    fn yuy2_saturated_red() {
        // 100% red, BT.601: Y=81, U=90, V=240 (two pixels).
        let src = [81, 90, 81, 240, 81, 90, 81, 240];
        // Note: YUY2 byte order is Y0 U Y1 V.
        let src = [
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ];
        let mut dst = [0u8; 12];
        assert!(yuy2_to_rgb24(&src, &mut dst, 2, 2));
        for px in dst.chunks_exact(3) {
            assert!(px[0] > 240, "R high: {px:?}");
            assert!(px[1] < 15, "G low: {px:?}");
            assert!(px[2] < 15, "B low: {px:?}");
        }
    }

    #[test]
    fn nv12_gray() {
        // 2x2 gray block: Y=128 everywhere, neutral chroma.
        let src = [128, 128, 128, 128, 128, 128];
        let mut dst = [0u8; 12];
        assert!(nv12_to_rgb24(&src, &mut dst, 2, 2));
        for px in dst.chunks_exact(3) {
            let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
            assert!((r - 130).abs() <= 2, "{r}");
            assert!((g - 130).abs() <= 2, "{g}");
            assert!((b - 130).abs() <= 2, "{b}");
        }
    }

    #[test]
    fn rejects_bad_sizes() {
        let src = [0u8; 8];
        let mut dst = [0u8; 6];
        assert!(!yuy2_to_rgb24(&src, &mut dst, 10, 10));
        assert!(!nv12_to_rgb24(&src, &mut dst, 10, 10));
    }

    #[test]
    fn rgb24_to_nv12_gray() {
        // 2x2 mid-gray block -> Y ~= 126, chroma ~= neutral (128).
        let src = [128u8; 2 * 2 * 3];
        let mut dst = [0u8; 2 * 2 * 3 / 2];
        assert!(rgb24_to_nv12(&src, &mut dst, 2, 2));
        for &y in &dst[0..4] {
            assert!((y as i32 - 126).abs() <= 3, "Y={y}");
        }
        assert!((dst[4] as i32 - 128).abs() <= 2, "U={}", dst[4]);
        assert!((dst[5] as i32 - 128).abs() <= 2, "V={}", dst[5]);
    }

    #[test]
    fn rgb24_to_nv12_red_roundtrip() {
        // Pure red -> NV12 -> back to RGB should stay red-ish.
        let src = [255u8, 0, 0].repeat(2 * 2);
        let mut nv12 = [0u8; 2 * 2 * 3 / 2];
        assert!(rgb24_to_nv12(&src, &mut nv12, 2, 2));
        let mut rgb = [0u8; 2 * 2 * 3];
        assert!(nv12_to_rgb24(&nv12, &mut rgb, 2, 2));
        for px in rgb.chunks_exact(3) {
            assert!(px[0] > 220, "R high: {px:?}");
            assert!(px[1] < 40, "G low: {px:?}");
            assert!(px[2] < 40, "B low: {px:?}");
        }
    }

    #[test]
    fn rgb24_to_nv12_rejects_odd_dims() {
        let src = [0u8; 3 * 3 * 3];
        let mut dst = [0u8; 3 * 3 * 3 / 2 + 1];
        assert!(!rgb24_to_nv12(&src, &mut dst, 3, 3));
    }

    #[test]
    fn rgb24_to_yuy2_gray() {
        let src = [128u8; 2 * 2 * 3];
        let mut dst = [0u8; 2 * 2 * 2];
        assert!(rgb24_to_yuy2(&src, &mut dst, 2, 2));
        // Y samples ~126, U/V ~128 for mid-gray.
        assert!((dst[0] as i32 - 126).abs() <= 3, "Y0={}", dst[0]);
        assert!((dst[1] as i32 - 128).abs() <= 2, "U={}", dst[1]);
        assert!((dst[2] as i32 - 126).abs() <= 3, "Y1={}", dst[2]);
        assert!((dst[3] as i32 - 128).abs() <= 2, "V={}", dst[3]);
    }
}
