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
    pub seq: u64,
    /// Round trip of the request that produced this frame.
    pub rtt: Duration,
    pub received: Instant,
}

impl LiveFrame {
    pub fn decode(yuv_plane: &[u8], key_c: [f32; 3], seq: u64, rtt: Duration) -> Self {
        let mut rgba = Vec::with_capacity(RGBA_LEN);
        yuv::yuv420_to_rgba(yuv_plane, &mut rgba);
        Self {
            rgba,
            yuv: yuv_plane.to_vec(),
            key_c,
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
