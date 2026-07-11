//! Pixel-format conversion: every captured [`VideoFrame`] becomes planar
//! I420 (4:2:0), the one input format the H.264 encoder consumes.
//!
//! The converter is deliberately dumb and total: one entry point,
//! [`to_i420`], that dispatches on [`PixelFormat`] and writes into a caller
//! owned [`I420Buffer`] scratch. The buffer grows to the largest resolution
//! seen and is then reused, so steady-state conversion performs zero
//! allocations per frame.
//!
//! Color-space conventions: the pipeline standardizes on **limited-range
//! BT.601** (Y ∈ [16, 235], chroma ∈ [16, 240]) because that is what UVC
//! cameras emit for YUYV/NV12 and what H.264 decoders assume by default.
//! RGB inputs are converted with the BT.601 matrix directly into limited
//! range; JPEG output (JFIF is full-range by definition) is range-compressed
//! so every path agrees.

use joy_capture::{PixelFormat, VideoFrame};
use openh264::formats::YUVSource;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::error::{MediaError, Result};

/// Reusable planar I420 scratch buffer.
///
/// Holds three planes (full-resolution Y, quarter-resolution U and V) plus
/// the dimensions they currently describe. [`to_i420`] resizes it on demand;
/// the backing vectors only ever grow, so a long-running encoder task
/// allocates once per resolution, not once per frame.
///
/// Implements openh264's [`YUVSource`] so it can be handed to the encoder
/// directly, with tightly packed planes (stride == width).
#[derive(Debug, Default)]
pub struct I420Buffer {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Buffer {
    /// Creates an empty buffer; the first conversion sizes it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resizes the planes for a `width` × `height` frame, reusing capacity.
    fn prepare(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        self.u.resize(width * height / 4, 0);
        self.v.resize(width * height / 4, 0);
    }
}

impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Converts a captured frame into planar I420 inside `buf`.
///
/// Supported inputs: `Rgb24`, `Yuyv`, `Nv12`, `Luma8`, and `Mjpeg` (decoded
/// with `zune-jpeg`; the JPEG's own dimensions win over the frame header's if
/// they disagree). Frames with odd dimensions are rejected — 4:2:0 chroma
/// requires even width and height, and every real camera mode satisfies that
/// — as are payloads whose length does not match their declared format.
pub fn to_i420(frame: &VideoFrame, buf: &mut I420Buffer) -> Result<()> {
    let width = frame.width as usize;
    let height = frame.height as usize;

    if frame.format != PixelFormat::Mjpeg {
        if width == 0 || height == 0 {
            return Err(MediaError::Convert(format!(
                "zero dimension: {width}x{height}"
            )));
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(MediaError::Convert(format!(
                "odd dimensions unsupported for 4:2:0 output: {width}x{height}"
            )));
        }
    }

    match frame.format {
        PixelFormat::Rgb24 => rgb24_to_i420(&frame.data, width, height, buf),
        PixelFormat::Yuyv => yuyv_to_i420(&frame.data, width, height, buf),
        PixelFormat::Nv12 => nv12_to_i420(&frame.data, width, height, buf),
        PixelFormat::Luma8 => luma8_to_i420(&frame.data, width, height, buf),
        PixelFormat::Mjpeg => mjpeg_to_i420(&frame.data, buf),
        other => Err(MediaError::Convert(format!(
            "unsupported pixel format: {other:?}"
        ))),
    }
}

/// Checks that `data` is exactly `expected` bytes for the given format label.
fn check_len(data: &[u8], expected: usize, format: &str) -> Result<()> {
    if data.len() != expected {
        return Err(MediaError::Convert(format!(
            "{format} payload is {} bytes, expected {expected}",
            data.len()
        )));
    }
    Ok(())
}

/// BT.601 limited-range luma from full-range RGB (integer approximation).
#[inline]
fn rgb_to_y(r: u16, g: u16, b: u16) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

/// BT.601 limited-range Cb from full-range RGB.
#[inline]
fn rgb_to_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8
}

/// BT.601 limited-range Cr from full-range RGB.
#[inline]
fn rgb_to_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8
}

fn rgb24_to_i420(data: &[u8], width: usize, height: usize, buf: &mut I420Buffer) -> Result<()> {
    check_len(data, width * height * 3, "RGB24")?;
    buf.prepare(width, height);

    for (i, px) in data.chunks_exact(3).enumerate() {
        buf.y[i] = rgb_to_y(px[0] as u16, px[1] as u16, px[2] as u16);
    }

    // Chroma from the average RGB of each 2x2 block, then one matrix step —
    // averaging before the matrix keeps the integer math exact and cheap.
    let chroma_w = width / 2;
    for cy in 0..height / 2 {
        for cx in 0..chroma_w {
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((cy * 2 + dy) * width + cx * 2 + dx) * 3;
                    r += data[p] as i32;
                    g += data[p + 1] as i32;
                    b += data[p + 2] as i32;
                }
            }
            let (r, g, b) = ((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
            buf.u[cy * chroma_w + cx] = rgb_to_u(r, g, b);
            buf.v[cy * chroma_w + cx] = rgb_to_v(r, g, b);
        }
    }
    Ok(())
}

fn yuyv_to_i420(data: &[u8], width: usize, height: usize, buf: &mut I420Buffer) -> Result<()> {
    check_len(data, width * height * 2, "YUYV")?;
    buf.prepare(width, height);

    // Packed 4:2:2: [Y0 U Y1 V] per horizontal pixel pair. Luma copies
    // straight through; chroma is already horizontally subsampled, so 4:2:0
    // only needs each vertical row pair averaged.
    let row_bytes = width * 2;
    for y in 0..height {
        let row = &data[y * row_bytes..(y + 1) * row_bytes];
        for (x, quad) in row.chunks_exact(4).enumerate() {
            buf.y[y * width + x * 2] = quad[0];
            buf.y[y * width + x * 2 + 1] = quad[2];
        }
    }

    let chroma_w = width / 2;
    for cy in 0..height / 2 {
        let top = &data[(cy * 2) * row_bytes..(cy * 2 + 1) * row_bytes];
        let bottom = &data[(cy * 2 + 1) * row_bytes..(cy * 2 + 2) * row_bytes];
        for cx in 0..chroma_w {
            let q = cx * 4;
            buf.u[cy * chroma_w + cx] =
                (top[q + 1] as u16 + bottom[q + 1] as u16).div_ceil(2) as u8;
            buf.v[cy * chroma_w + cx] =
                (top[q + 3] as u16 + bottom[q + 3] as u16).div_ceil(2) as u8;
        }
    }
    Ok(())
}

fn nv12_to_i420(data: &[u8], width: usize, height: usize, buf: &mut I420Buffer) -> Result<()> {
    check_len(data, width * height * 3 / 2, "NV12")?;
    buf.prepare(width, height);

    buf.y.copy_from_slice(&data[..width * height]);

    // NV12's chroma is already 4:2:0, just interleaved: deinterleave the UV
    // plane into separate U and V planes.
    let uv = &data[width * height..];
    for (i, pair) in uv.chunks_exact(2).enumerate() {
        buf.u[i] = pair[0];
        buf.v[i] = pair[1];
    }
    Ok(())
}

fn luma8_to_i420(data: &[u8], width: usize, height: usize, buf: &mut I420Buffer) -> Result<()> {
    check_len(data, width * height, "Luma8")?;
    buf.prepare(width, height);
    buf.y.copy_from_slice(data);
    buf.u.fill(128);
    buf.v.fill(128);
    Ok(())
}

/// Compresses a full-range (JFIF) luma sample to limited range.
#[inline]
fn full_to_limited_y(y: u8) -> u8 {
    (16 + (y as u32 * 219 + 127) / 255) as u8
}

/// Compresses a full-range (JFIF) chroma sample to limited range.
#[inline]
fn full_to_limited_c(c: u8) -> u8 {
    (16 + (c as u32 * 224 + 127) / 255) as u8
}

fn mjpeg_to_i420(data: &[u8], buf: &mut I420Buffer) -> Result<()> {
    // Ask the decoder for YCbCr so no RGB round-trip happens; the output is
    // interleaved 4:4:4 which we subsample below. JFIF YCbCr is full-range,
    // so every sample is compressed to the pipeline's limited range.
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr);
    let mut decoder = JpegDecoder::new_with_options(std::io::Cursor::new(data), options);
    let pixels = decoder
        .decode()
        .map_err(|e| MediaError::Convert(format!("MJPEG decode failed: {e}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| MediaError::Convert("MJPEG decode produced no image info".into()))?;

    let width = info.width as usize;
    let height = info.height as usize;
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(MediaError::Convert(format!(
            "MJPEG dimensions unsupported for 4:2:0 output: {width}x{height}"
        )));
    }
    check_len(&pixels, width * height * 3, "decoded YCbCr")?;
    buf.prepare(width, height);

    for (i, px) in pixels.chunks_exact(3).enumerate() {
        buf.y[i] = full_to_limited_y(px[0]);
    }

    let chroma_w = width / 2;
    for cy in 0..height / 2 {
        for cx in 0..chroma_w {
            let (mut cb, mut cr) = (0u32, 0u32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((cy * 2 + dy) * width + cx * 2 + dx) * 3;
                    cb += pixels[p + 1] as u32;
                    cr += pixels[p + 2] as u32;
                }
            }
            buf.u[cy * chroma_w + cx] = full_to_limited_c(((cb + 2) / 4) as u8);
            buf.v[cy * chroma_w + cx] = full_to_limited_c(((cr + 2) / 4) as u8);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use joy_core::Timestamp;

    /// Builds a frame with the given format and payload for a 4x2 image.
    fn frame(format: PixelFormat, width: u32, height: u32, data: Vec<u8>) -> VideoFrame {
        VideoFrame {
            ts: Timestamp::now(),
            format,
            width,
            height,
            data: Bytes::from(data),
        }
    }

    #[test]
    fn rgb24_solid_white_hits_bt601_limits() {
        let f = frame(PixelFormat::Rgb24, 4, 2, vec![255; 4 * 2 * 3]);
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        // Limited-range white: Y=235, chroma neutral 128 (±1 for rounding).
        assert!(
            buf.y().iter().all(|&y| (234..=236).contains(&y)),
            "{:?}",
            buf.y()
        );
        assert!(buf.u().iter().all(|&u| (127..=129).contains(&u)));
        assert!(buf.v().iter().all(|&v| (127..=129).contains(&v)));
    }

    #[test]
    fn rgb24_solid_black_hits_bt601_limits() {
        let f = frame(PixelFormat::Rgb24, 4, 2, vec![0; 4 * 2 * 3]);
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        assert!(buf.y().iter().all(|&y| (15..=17).contains(&y)));
        assert!(buf.u().iter().all(|&u| (127..=129).contains(&u)));
        assert!(buf.v().iter().all(|&v| (127..=129).contains(&v)));
    }

    #[test]
    fn rgb24_pure_red_has_high_cr() {
        let f = frame(PixelFormat::Rgb24, 4, 2, [255u8, 0, 0].repeat(4 * 2));
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        // BT.601 red: Y≈81, Cb≈90, Cr≈240.
        assert!(
            buf.y().iter().all(|&y| (79..=83).contains(&y)),
            "{:?}",
            buf.y()
        );
        assert!(
            buf.u().iter().all(|&u| (88..=92).contains(&u)),
            "{:?}",
            buf.u()
        );
        assert!(
            buf.v().iter().all(|&v| (238..=241).contains(&v)),
            "{:?}",
            buf.v()
        );
    }

    #[test]
    fn yuyv_copies_luma_and_averages_chroma_rows() {
        // 2x2 image, 2 rows of one pixel pair each.
        // Row 0: Y0=10 U=100 Y1=20 V=200; row 1: Y0=30 U=120 Y1=40 V=220.
        let f = frame(
            PixelFormat::Yuyv,
            2,
            2,
            vec![10, 100, 20, 200, 30, 120, 40, 220],
        );
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        assert_eq!(buf.y(), &[10, 20, 30, 40]);
        assert_eq!(buf.u(), &[110]); // avg(100, 120)
        assert_eq!(buf.v(), &[210]); // avg(200, 220)
    }

    #[test]
    fn nv12_deinterleaves_chroma() {
        // 2x2: Y plane [1,2,3,4], UV plane [U=50, V=60].
        let f = frame(PixelFormat::Nv12, 2, 2, vec![1, 2, 3, 4, 50, 60]);
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        assert_eq!(buf.y(), &[1, 2, 3, 4]);
        assert_eq!(buf.u(), &[50]);
        assert_eq!(buf.v(), &[60]);
    }

    #[test]
    fn luma8_fills_neutral_chroma() {
        let f = frame(PixelFormat::Luma8, 2, 2, vec![9, 8, 7, 6]);
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");

        assert_eq!(buf.y(), &[9, 8, 7, 6]);
        assert_eq!(buf.u(), &[128]);
        assert_eq!(buf.v(), &[128]);
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let f = frame(PixelFormat::Rgb24, 3, 2, vec![0; 3 * 2 * 3]);
        let mut buf = I420Buffer::new();
        assert!(matches!(to_i420(&f, &mut buf), Err(MediaError::Convert(_))));
    }

    #[test]
    fn short_payload_is_rejected() {
        let f = frame(PixelFormat::Rgb24, 4, 2, vec![0; 5]);
        let mut buf = I420Buffer::new();
        assert!(matches!(to_i420(&f, &mut buf), Err(MediaError::Convert(_))));
    }

    #[test]
    fn buffer_is_reused_across_resolutions() {
        let mut buf = I420Buffer::new();
        let big = frame(PixelFormat::Luma8, 4, 4, vec![1; 16]);
        to_i420(&big, &mut buf).expect("convert big");
        assert_eq!(buf.dimensions(), (4, 4));

        let small = frame(PixelFormat::Luma8, 2, 2, vec![2; 4]);
        to_i420(&small, &mut buf).expect("convert small");
        assert_eq!(buf.dimensions(), (2, 2));
        assert_eq!(buf.y(), &[2, 2, 2, 2]);
    }

    #[test]
    fn mjpeg_decodes_to_i420() {
        // A tiny valid JPEG produced by encoding an 16x16 gray image; checked
        // in as a fixture to avoid a jpeg *encoder* dependency.
        let jpeg = include_bytes!("../tests/fixtures/gray16x16.jpg");
        let f = VideoFrame {
            ts: Timestamp::now(),
            format: PixelFormat::Mjpeg,
            width: 16,
            height: 16,
            data: Bytes::from_static(jpeg),
        };
        let mut buf = I420Buffer::new();
        to_i420(&f, &mut buf).expect("convert");
        assert_eq!(buf.dimensions(), (16, 16));

        // The fixture is mid-gray: luma clustered near 128 scaled to limited
        // range (≈126), chroma neutral.
        let y_avg: u32 = buf.y().iter().map(|&y| y as u32).sum::<u32>() / 256;
        assert!((115..=140).contains(&y_avg), "y_avg={y_avg}");
        assert!(buf.u().iter().all(|&u| (120..=136).contains(&u)));
        assert!(buf.v().iter().all(|&v| (120..=136).contains(&v)));
    }
}
