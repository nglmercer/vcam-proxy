//! Resolution scaling support for v4l2loopback output.

use crate::frame::PixelFormat;

/// Pre-computed index LUT for fast nearest-neighbor scaling.
pub struct ScaleLUT {
    /// Maps dst x -> src x for Y plane
    x_lut: Vec<usize>,
    /// Maps dst y -> src y for Y plane
    y_lut: Vec<usize>,
    /// Maps dst uv x -> src uv x for UV plane
    uv_x_lut: Vec<usize>,
    /// Maps dst uv y -> src uv y for UV plane
    uv_y_lut: Vec<usize>,
}

impl ScaleLUT {
    fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        let mut x_lut = Vec::with_capacity(dst_w as usize);
        let mut y_lut = Vec::with_capacity(dst_h as usize);
        let mut uv_x_lut = Vec::with_capacity(dst_w as usize);
        let mut uv_y_lut = Vec::with_capacity((dst_h / 2) as usize);

        // Luma (Y) plane LUTs
        for x in 0..dst_w as usize {
            x_lut.push((x * src_w as usize) / dst_w as usize);
        }
        for y in 0..dst_h as usize {
            y_lut.push((y * src_h as usize) / dst_h as usize);
        }

        // Chroma (UV) plane LUTs - NV12 UV is at half vertical resolution
        for x in 0..dst_w as usize {
            uv_x_lut.push((x * src_w as usize) / dst_w as usize);
        }
        for y in 0..(dst_h / 2) as usize {
            uv_y_lut.push((y * (src_h / 2) as usize) / (dst_h / 2) as usize);
        }

        ScaleLUT {
            x_lut,
            y_lut,
            uv_x_lut,
            uv_y_lut,
        }
    }
}

/// Scaling context with pre-computed LUTs and reusable buffers.
/// Eliminates per-frame allocations and division operations.
pub struct ScaleContext {
    lut: ScaleLUT,
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
}

impl ScaleContext {
    pub(crate) fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        ScaleContext {
            lut: ScaleLUT::new(src_w, src_h, dst_w, dst_h),
            src_w: src_w as usize,
            src_h: src_h as usize,
            dst_w: dst_w as usize,
            dst_h: dst_h as usize,
        }
    }

    /// Scale NV12 frame using pre-computed LUTs.
    /// Returns false if buffer sizes are insufficient.
    #[inline]
    pub(crate) fn scale_nv12(&self, src: &[u8], dst: &mut [u8]) -> bool {
        let src_y_size = self.src_w * self.src_h;
        let dst_y_size = self.dst_w * self.dst_h;
        if src.len() < src_y_size * 3 / 2 || dst.len() < dst_y_size * 3 / 2 {
            return false;
        }

        let (src_y, src_uv) = src.split_at(src_y_size);
        let (dst_y, dst_uv) = dst.split_at_mut(dst_y_size);

        // Scale Y plane (luma) - uses LUT for O(1) coordinate lookup
        for dy in 0..self.dst_h {
            let sy = self.lut.y_lut[dy];
            let src_row_start = sy * self.src_w;
            let dst_row_start = dy * self.dst_w;

            for dx in 0..self.dst_w {
                let sx = self.lut.x_lut[dx];
                dst_y[dst_row_start + dx] = src_y[src_row_start + sx];
            }
        }

        // Scale UV plane (chroma) - NV12 has interleaved UV at half resolution
        let dst_uv_h = self.dst_h / 2;
        for dy in 0..dst_uv_h {
            let sy = self.lut.uv_y_lut[dy];
            let src_row_start = sy * self.src_w;
            let dst_row_start = dy * self.dst_w;

            for dx in 0..self.dst_w {
                let sx = self.lut.uv_x_lut[dx];
                dst_uv[dst_row_start + dx] = src_uv[src_row_start + sx];
            }
        }
        true
    }
}

/// Convert a V4L2 FourCC byte array to our PixelFormat.
/// Returns None for formats we don't support.
pub(crate) fn fourcc_to_pixel_format(fourcc: &[u8; 4]) -> Option<PixelFormat> {
    match fourcc {
        b"YUYV" => Some(PixelFormat::Yuy2),
        b"RGB3" => Some(PixelFormat::Rgb24),
        b"NV12" => Some(PixelFormat::Nv12),
        b"MJPG" => Some(PixelFormat::Mjpeg),
        _ => None,
    }
}
