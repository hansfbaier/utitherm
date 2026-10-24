//! YUV 4:2:0 → RGBA8 conversion for live frames (§5.1).
//!
//! The device already palette-maps colour into the YUV planes, so this is a
//! plain BT.601 full-range conversion; there is no thermal palette step here.
//!
//! ```text
//! R = Y + 1.402*(V-128)
//! G = Y - 0.344136*(U-128) - 0.714136*(V-128)
//! B = Y + 1.772*(U-128)
//! ```

use crate::protocol::{FRAME_H, FRAME_W, RGBA_LEN, YUV_CHROMA_LEN, YUV_LEN, YUV_Y_LEN};

/// Convert one 115 200-byte YUV 4:2:0 frame into `out` as 24-bit RGBA (`a=255`).
///
/// `out` is cleared and filled with exactly [`RGBA_LEN`] bytes.
pub fn yuv420_to_rgba(yuv: &[u8], out: &mut Vec<u8>) {
    assert_eq!(yuv.len(), YUV_LEN, "unexpected YUV frame size");
    out.clear();
    out.reserve(RGBA_LEN);

    let (y_plane, chroma) = yuv.split_at(YUV_Y_LEN);
    let (u_plane, v_plane) = chroma.split_at(YUV_CHROMA_LEN);
    let chroma_w = FRAME_W / 2;

    for y in 0..FRAME_H {
        let luma_row = y * FRAME_W;
        let chroma_row = (y / 2) * chroma_w;
        for x in 0..FRAME_W {
            let luma = f32::from(y_plane[luma_row + x]);
            let c = chroma_row + x / 2;
            let u = f32::from(u_plane[c]) - 128.0;
            let v = f32::from(v_plane[c]) - 128.0;

            let r = luma + 1.402 * v;
            let g = luma - 0.344_136 * u - 0.714_136 * v;
            let b = luma + 1.772 * u;

            out.push(clamp_u8(r));
            out.push(clamp_u8(g));
            out.push(clamp_u8(b));
            out.push(255);
        }
    }
}

#[inline]
fn clamp_u8(value: f32) -> u8 {
    // `round` keeps us well inside the ±1 tolerance of the golden fixture (§13.3).
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(yuv: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * FRAME_W + x) * 4;
        [yuv[i], yuv[i + 1], yuv[i + 2], yuv[i + 3]]
    }

    /// Neutral chroma is a pure luma ramp.
    #[test]
    fn neutral_grey_passthrough() {
        let mut yuv = vec![0u8; YUV_LEN];
        let y = &mut yuv[..YUV_Y_LEN];
        for (i, slot) in y.iter_mut().enumerate() {
            *slot = (i % 256) as u8;
        }
        for slot in &mut yuv[YUV_Y_LEN..] {
            *slot = 128;
        }
        let mut rgba = Vec::new();
        yuv420_to_rgba(&yuv, &mut rgba);

        for (i, px) in rgba.as_chunks::<4>().0.iter().enumerate() {
            let expected = (i % 256) as u8;
            assert_eq!(*px, [expected, expected, expected, 255], "pixel {i}");
        }
    }

    /// BT.601 red: Y=76, U=85, V=255 → (254, 0, 0).
    #[test]
    fn bt601_reference_red() {
        let mut yuv = vec![0u8; YUV_LEN];
        yuv[..YUV_Y_LEN].fill(76);
        yuv[YUV_Y_LEN..YUV_Y_LEN + YUV_CHROMA_LEN].fill(85);
        yuv[YUV_Y_LEN + YUV_CHROMA_LEN..].fill(255);

        let mut rgba = Vec::new();
        yuv420_to_rgba(&yuv, &mut rgba);
        assert_eq!(pixel(&rgba, 0, 0), [254, 0, 0, 255]);
        assert_eq!(pixel(&rgba, FRAME_W - 1, FRAME_H - 1), [254, 0, 0, 255]);
    }

    /// Chroma subsampling: every 2x2 block shares one U/V pair.
    #[test]
    fn chroma_blocks_are_shared() {
        let mut yuv = vec![128u8; YUV_LEN];
        yuv[..YUV_Y_LEN].fill(128);
        yuv[YUV_Y_LEN..].fill(128);
        // First chroma sample (covers x=0..1, y=0..1) pushed to pure blue-ish.
        yuv[YUV_Y_LEN] = 240; // U
        yuv[YUV_Y_LEN + YUV_CHROMA_LEN] = 128; // V

        let mut rgba = Vec::new();
        yuv420_to_rgba(&yuv, &mut rgba);

        let blue = pixel(&rgba, 0, 0);
        assert_eq!(blue, pixel(&rgba, 1, 0));
        assert_eq!(blue, pixel(&rgba, 0, 1));
        assert_eq!(blue, pixel(&rgba, 1, 1));
        assert!(blue[2] > blue[0], "U should raise blue: {blue:?}");
        // Next block is back to neutral grey.
        assert_eq!(pixel(&rgba, 2, 0), [128, 128, 128, 255]);
    }

    /// Full matrix sweep against an independently computed reference.
    #[test]
    fn matrix_matches_reference_formula() {
        let yuv = crate::fixtures::yuv_frame();
        let mut rgba = Vec::new();
        yuv420_to_rgba(&yuv, &mut rgba);

        let cw = FRAME_W / 2;
        for (x, y) in [(0usize, 0usize), (1, 1), (37, 200), (239, 319), (120, 160)] {
            let luma = f32::from(yuv[y * FRAME_W + x]);
            let c = (y / 2) * cw + x / 2;
            let u = f32::from(yuv[YUV_Y_LEN + c]) - 128.0;
            let v = f32::from(yuv[YUV_Y_LEN + YUV_CHROMA_LEN + c]) - 128.0;
            let expect = [
                (luma + 1.402 * v).round().clamp(0.0, 255.0) as u8,
                (luma - 0.344_136 * u - 0.714_136 * v)
                    .round()
                    .clamp(0.0, 255.0) as u8,
                (luma + 1.772 * u).round().clamp(0.0, 255.0) as u8,
                255,
            ];
            assert_eq!(pixel(&rgba, x, y), expect, "pixel ({x},{y})");
        }
    }
}
