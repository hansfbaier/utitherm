//! One decoded live frame: pixel planes, key temperatures and transport stats.

use std::time::{Duration, Instant};

use crate::model::{self, PixelFormat};
use crate::yuv;

#[derive(Debug)]
pub struct LiveFrame {
    /// Display-sized RGBA8, row-major, alpha = 255.
    pub rgba: Vec<u8>,
    /// Raw pixel block: I420 or interleaved RGB888, kept for export and fixtures.
    pub yuv: Vec<u8>,
    pub format: PixelFormat,
    pub display_w: usize,
    pub display_h: usize,
    pub raw_w: usize,
    pub raw_h: usize,
    /// Three key temperatures from the frame trailer. Ordering is believed
    /// max/min/centre but unverified (§5.2, E4).
    pub key_c: [f32; 3],
    /// Live temperature plane (f32 LE) if this reply carried one.
    pub plane_c: Option<Vec<f32>>,
    pub seq: u64,
    /// Round trip of the request that produced this frame.
    pub rtt: Duration,
    pub received: Instant,
}

impl LiveFrame {
    /// Decode the reference-model geometry. Used by tests and raw fixtures.
    pub fn decode(
        yuv_plane: &[u8],
        key_c: [f32; 3],
        live_plane: Option<&[u8]>,
        seq: u64,
        rtt: Duration,
    ) -> Self {
        let geometry = model::REFERENCE;
        Self::decode_with(
            yuv_plane,
            geometry.format,
            geometry.display_w,
            geometry.display_h,
            geometry.raw_w,
            geometry.raw_h,
            key_c,
            live_plane,
            seq,
            rtt,
        )
    }

    /// Decode one frame with the geometry selected by the family table.
    #[allow(clippy::too_many_arguments)]
    pub fn decode_with(
        pixels: &[u8],
        format: PixelFormat,
        display_w: usize,
        display_h: usize,
        raw_w: usize,
        raw_h: usize,
        key_c: [f32; 3],
        live_plane: Option<&[u8]>,
        seq: u64,
        rtt: Duration,
    ) -> Self {
        let mut rgba = Vec::with_capacity(display_w * display_h * 4);
        match format {
            PixelFormat::I420 => yuv::yuv420_to_rgba_dims(pixels, display_w, display_h, &mut rgba),
            PixelFormat::Rgb888 => yuv::rgb888_to_rgba(pixels, display_w, display_h, &mut rgba),
        }
        let plane_c = live_plane.and_then(|bytes| {
            if bytes.len() != raw_w * raw_h * 4 {
                return None;
            }
            Some(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| f32::from_le_bytes(*chunk))
                    .collect(),
            )
        });
        Self {
            rgba,
            yuv: pixels.to_vec(),
            format,
            display_w,
            display_h,
            raw_w,
            raw_h,
            key_c,
            plane_c,
            seq,
            rtt,
            received: Instant::now(),
        }
    }

    /// Key temperature for display; `None` when the source has no temperature
    /// data (raw `.yuv` replay fixtures).
    pub fn key(&self, index: usize) -> Option<f32> {
        self.key_c.get(index).copied().filter(|v| v.is_finite())
    }

    pub fn display_len(&self) -> usize {
        self.display_w * self.display_h
    }

    pub fn raw_samples(&self) -> usize {
        self.raw_w * self.raw_h
    }
}

/// Heuristic flat-field-correction boundary detector (§5.5).
///
/// The shutter cycle shifts all three key temperatures together while the
/// scene structure stays put. That step is a recalibration, not a scene change,
/// so frames around it must not enter a measurement baseline.
#[derive(Debug, Default)]
pub struct FfcDetector {
    previous: Option<([f32; 3], f32)>,
}

/// Minimum simultaneous key-temperature step that counts as an FFC boundary.
pub const FFC_STEP_C: f32 = 0.3;
/// Maximum mean-luma change allowed for the scene to still count as static.
pub const FFC_STATIC_LUMA: f32 = 4.0;

impl FfcDetector {
    /// Observe one frame; returns true when it looks like an FFC boundary.
    pub fn observe(&mut self, frame: &LiveFrame) -> bool {
        let keys = frame.key_c;
        if !keys.iter().all(|value| value.is_finite()) {
            self.previous = None;
            return false;
        }
        let luma = mean_luma(frame);
        let previous = self.previous.replace((keys, luma));
        let Some((previous_keys, previous_luma)) = previous else {
            return false;
        };
        let steps = [
            keys[0] - previous_keys[0],
            keys[1] - previous_keys[1],
            keys[2] - previous_keys[2],
        ];
        let all_up = steps.iter().all(|step| *step > FFC_STEP_C);
        let all_down = steps.iter().all(|step| *step < -FFC_STEP_C);
        let static_scene = (luma - previous_luma).abs() <= FFC_STATIC_LUMA;
        (all_up || all_down) && static_scene
    }
}

fn mean_luma(frame: &LiveFrame) -> f32 {
    let display_pixels = frame.display_w * frame.display_h;
    match frame.format {
        PixelFormat::I420 => {
            let luma = &frame.yuv[..display_pixels.min(frame.yuv.len())];
            if luma.is_empty() {
                return 0.0;
            }
            luma.iter().map(|&value| f32::from(value)).sum::<f32>() / luma.len() as f32
        }
        PixelFormat::Rgb888 => {
            let pixels = frame.yuv.as_chunks::<3>().0;
            if pixels.is_empty() {
                return 0.0;
            }
            pixels
                .iter()
                .map(|p| {
                    0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2])
                })
                .sum::<f32>()
                / pixels.len() as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{RGBA_LEN, YUV_LEN};

    #[test]
    fn reference_frame_has_display_rgba() {
        let yuv = crate::fixtures::yuv_frame();
        let frame = LiveFrame::decode(&yuv, [30.0, 31.0, 32.0], None, 1, Duration::ZERO);
        assert_eq!(frame.rgba.len(), RGBA_LEN);
        assert_eq!(frame.yuv.len(), YUV_LEN);
        assert_eq!(frame.format, PixelFormat::I420);
        assert_eq!((frame.display_w, frame.display_h), (240, 320));
        assert_eq!((frame.raw_w, frame.raw_h), (192, 256));
    }

    fn frame(keys: [f32; 3], luma: u8) -> LiveFrame {
        let mut yuv = vec![luma; crate::protocol::YUV_LEN];
        yuv[crate::protocol::YUV_Y_LEN..].fill(128);
        LiveFrame::decode(&yuv, keys, None, 1, Duration::from_millis(1))
    }

    #[test]
    fn decodes_rgb888_at_non_reference_geometry() {
        let width = 640;
        let height = 480;
        let mut pixels = vec![0u8; width * height * 3];
        pixels[0..3].copy_from_slice(&[10, 20, 30]);
        let frame = LiveFrame::decode_with(
            &pixels,
            PixelFormat::Rgb888,
            width,
            height,
            320,
            240,
            [30.0, 31.0, 32.0],
            None,
            1,
            Duration::ZERO,
        );
        assert_eq!(frame.rgba.len(), width * height * 4);
        assert_eq!(&frame.rgba[0..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn detects_a_simultaneous_step_on_a_static_scene() {
        let mut detector = FfcDetector::default();
        assert!(!detector.observe(&frame([30.0, 31.0, 32.0], 100)));
        assert!(detector.observe(&frame([30.5, 31.5, 32.5], 101)));
    }

    #[test]
    fn ignores_a_step_when_the_scene_changes() {
        let mut detector = FfcDetector::default();
        assert!(!detector.observe(&frame([30.0, 31.0, 32.0], 60)));
        assert!(!detector.observe(&frame([30.5, 31.5, 32.5], 120)));
    }

    #[test]
    fn ignores_a_single_reading_change() {
        let mut detector = FfcDetector::default();
        assert!(!detector.observe(&frame([30.0, 31.0, 32.0], 100)));
        assert!(!detector.observe(&frame([30.0, 31.5, 32.0], 100)));
    }
}
