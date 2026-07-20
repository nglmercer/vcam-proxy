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
        let src = [src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7]];
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
}
