//! One decoded live frame: pixel planes, key temperatures and transport stats.

use std::time::{Duration, Instant};

use crate::protocol::RGBA_LEN;
use crate::yuv;

#[derive(Debug)]
pub struct LiveFrame {
    /// 240x320 RGBA8, row-major, alpha = 255.
    pub rgba: Vec<u8>,
    /// Raw 115 200-byte YUV 4:2:0 block, kept for `.yuv` export and fixtures.
    pub yuv: Vec<u8>,
    /// Three key temperatures from the frame trailer. Ordering is believed
    /// max/min/centre but unverified (§5.2, E4).
    pub key_c: [f32; 3],
    /// Live 192x256 temperature plane (f32 LE) if this reply was an extended
    /// frame (device sends one every 11th request).
    pub plane_c: Option<Vec<f32>>,
    pub seq: u64,
    /// Round trip of the request that produced this frame.
    pub rtt: Duration,
    pub received: Instant,
}

impl LiveFrame {
    pub fn decode(
        yuv_plane: &[u8],
        key_c: [f32; 3],
        live_plane: Option<&[u8]>,
        seq: u64,
        rtt: Duration,
    ) -> Self {
        let mut rgba = Vec::with_capacity(RGBA_LEN);
        yuv::yuv420_to_rgba(yuv_plane, &mut rgba);
        let plane_c = live_plane.and_then(|bytes| {
            if bytes.len() != crate::protocol::LIVE_PLANE_BYTES {
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
            yuv: yuv_plane.to_vec(),
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
        let luma = mean_luma(&frame.yuv);
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

fn mean_luma(yuv: &[u8]) -> f32 {
    let luma = &yuv[..crate::protocol::YUV_Y_LEN.min(yuv.len())];
    if luma.is_empty() {
        return 0.0;
    }
    luma.iter().map(|&value| f32::from(value)).sum::<f32>() / luma.len() as f32
}

#[cfg(test)]
mod ffc_tests {
    use super::*;
    use std::time::Duration;

    fn frame(keys: [f32; 3], luma: u8) -> LiveFrame {
        let mut yuv = vec![luma; crate::protocol::YUV_LEN];
        yuv[crate::protocol::YUV_Y_LEN..].fill(128);
        LiveFrame {
            rgba: Vec::new(),
            yuv,
            key_c: keys,
            plane_c: None,
            seq: 1,
            rtt: Duration::from_millis(1),
            received: std::time::Instant::now(),
        }
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
