//! `PhotoCap` capture decoding and measurement-plane math (§6).
//!
//! The reply is five `u32 BE` length-prefixed sections:
//!
//! | # | content                       | size                     |
//! |---|-------------------------------|--------------------------|
//! | 0 | JPEG with OSD                 | varies (240x320)         |
//! | 1 | f32 LE temperature, °C        | 196 608 (192 x 256)      |
//! | 2 | JPEG, clean image             | varies (480x640)         |
//! | 3 | u16 LE raw sensor plane       | 98 304 (192 x 256)       |
//! | 4 | 8 unknown bytes               | 8                        |
//!
//! The measurement plane is **192 wide x 256 tall** (row stride 192) and maps to
//! the 240x320 display with no rotation or flip at exactly 1.25
//! (`plane = display / 1.25`). The f32 plane is the temperature source; the u16
//! plane is not linear in temperature and is kept for diagnostics only.

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::time::Instant;

use crate::protocol::{self, FRAME_H, FRAME_W, ProtoError, Result, TOKEN_PHOTOCAP};

pub const TEMP_PLANE_W: usize = 192;
pub const TEMP_PLANE_H: usize = 256;
pub const TEMP_PLANE_SAMPLES: usize = TEMP_PLANE_W * TEMP_PLANE_H; // 49 152
pub const TEMP_PLANE_BYTES: usize = TEMP_PLANE_SAMPLES * 4; // 196 608
pub const Y16_PLANE_BYTES: usize = TEMP_PLANE_SAMPLES * 2; // 98 304
/// Display pixels per plane sample (§6.1).
pub const PLANE_TO_DISPLAY: f32 = 1.25;
/// Plane samples per display pixel.
pub const DISPLAY_TO_PLANE: f32 = 1.0 / PLANE_TO_DISPLAY;

#[derive(Clone, Debug)]
pub struct Capture {
    /// Section 0: JPEG 240x320 with the device OSD.
    pub shot_jpeg: Vec<u8>,
    /// Section 1: f32 LE °C, row-major 192x256.
    pub temperature_c: Vec<f32>,
    /// Section 2: JPEG 480x640, no OSD.
    pub vis_jpeg: Vec<u8>,
    /// Section 3: u16 LE raw sensor plane, 192x256.
    pub y16: Vec<u8>,
    /// Section 4: eight bytes of unknown purpose [U].
    pub tail: [u8; 8],
    pub received: Instant,
}

#[derive(Clone, Copy, Debug)]
pub struct TempStats {
    pub min: f32,
    pub max: f32,
    pub mean: f32,
    pub std_dev: f32,
}

impl Capture {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut sections: Vec<&[u8]> = Vec::with_capacity(5);
        let mut offset = 0usize;
        for _ in 0..5 {
            let Some(head) = payload.get(offset..offset + 4) else {
                return Err(ProtoError::Field {
                    name: "PhotoCap section table",
                    why: format!("payload ends at {offset} before a length word"),
                });
            };
            let len = u32::from_be_bytes(head.try_into().expect("slice of four")) as usize;
            offset += 4;
            let Some(body) = payload.get(offset..offset.saturating_add(len)) else {
                return Err(ProtoError::Field {
                    name: "PhotoCap section",
                    why: format!(
                        "section claims {len} B but only {} B remain",
                        payload.len().saturating_sub(offset)
                    ),
                });
            };
            sections.push(body);
            offset += len;
        }
        if offset != payload.len() {
            return Err(ProtoError::Field {
                name: "PhotoCap payload",
                why: format!("{} bytes after the fifth section", payload.len() - offset),
            });
        }

        let (shot_jpeg, temperature_raw, vis_jpeg, y16, tail_raw) = (
            sections[0],
            sections[1],
            sections[2],
            sections[3],
            sections[4],
        );

        if temperature_raw.len() != TEMP_PLANE_BYTES {
            return Err(ProtoError::BadPayloadLen {
                what: "PhotoCap temperature plane",
                got: temperature_raw.len(),
                want: TEMP_PLANE_BYTES,
            });
        }
        if y16.len() != Y16_PLANE_BYTES {
            return Err(ProtoError::BadPayloadLen {
                what: "PhotoCap y16 plane",
                got: y16.len(),
                want: Y16_PLANE_BYTES,
            });
        }
        if tail_raw.len() != 8 {
            return Err(ProtoError::BadPayloadLen {
                what: "PhotoCap trailing block",
                got: tail_raw.len(),
                want: 8,
            });
        }
        if !is_jpeg(shot_jpeg) {
            return Err(ProtoError::Field {
                name: "PhotoCap section 0",
                why: "missing JPEG SOI/EOI markers".to_owned(),
            });
        }
        if !is_jpeg(vis_jpeg) {
            return Err(ProtoError::Field {
                name: "PhotoCap section 2",
                why: "missing JPEG SOI/EOI markers".to_owned(),
            });
        }

        let mut temperature_c = Vec::with_capacity(TEMP_PLANE_SAMPLES);
        for (index, chunk) in temperature_raw.as_chunks::<4>().0.iter().enumerate() {
            let value = f32::from_le_bytes(*chunk);
            if !value.is_finite() {
                return Err(ProtoError::Field {
                    name: "PhotoCap temperature plane",
                    why: format!("non-finite sample {value} at index {index}"),
                });
            }
            temperature_c.push(value);
        }

        let mut tail = [0u8; 8];
        tail.copy_from_slice(tail_raw);

        Ok(Self {
            shot_jpeg: shot_jpeg.to_vec(),
            temperature_c,
            vis_jpeg: vis_jpeg.to_vec(),
            y16: y16.to_vec(),
            tail,
            received: Instant::now(),
        })
    }

    pub fn stats(&self) -> TempStats {
        plane_stats(&self.temperature_c).unwrap_or(TempStats {
            min: f32::NAN,
            max: f32::NAN,
            mean: f32::NAN,
            std_dev: f32::NAN,
        })
    }

    /// Sample the plane at a display pixel coordinate (0..240, 0..320).
    pub fn temperature_at_display(&self, display_x: f32, display_y: f32) -> Option<f32> {
        let (px, py) = plane_coords(display_x, display_y)?;
        self.temperature_at_plane(px, py)
    }

    pub fn temperature_at_plane(&self, px: usize, py: usize) -> Option<f32> {
        if px >= TEMP_PLANE_W || py >= TEMP_PLANE_H {
            return None;
        }
        self.temperature_c.get(py * TEMP_PLANE_W + px).copied()
    }

    pub fn write_temperature_plane(&self, path: &Path) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(TEMP_PLANE_BYTES);
        for value in &self.temperature_c {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        fs::write(path, bytes)
    }

    pub fn write_y16_plane(&self, path: &Path) -> std::io::Result<()> {
        fs::write(path, &self.y16)
    }

    /// Rebuild the bare section stream for a `PhotoCap` reply.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut temperature = Vec::with_capacity(self.temperature_c.len() * 4);
        for value in &self.temperature_c {
            temperature.extend_from_slice(&value.to_le_bytes());
        }
        let sections: [&[u8]; 5] = [
            &self.shot_jpeg,
            &temperature,
            &self.vis_jpeg,
            &self.y16,
            &self.tail,
        ];
        let mut payload = Vec::new();
        for section in sections {
            payload.extend_from_slice(&(section.len() as u32).to_be_bytes());
            payload.extend_from_slice(section);
        }
        payload
    }

    /// Rebuild the full framed `PhotoCap` reply.
    pub fn to_message(&self) -> Vec<u8> {
        protocol::encode_message(TOKEN_PHOTOCAP, &self.to_payload())
    }
}

/// Statistics for a 192x256 f32 plane (PhotoCap section 1 or a live plane).
pub fn plane_stats(plane: &[f32]) -> Option<TempStats> {
    if plane.len() != TEMP_PLANE_SAMPLES {
        return None;
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut count = 0u32;
    for &value in plane {
        if !value.is_finite() {
            continue;
        }
        min = min.min(value);
        max = max.max(value);
        sum += f64::from(value);
        sum_sq += f64::from(value) * f64::from(value);
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let n = f64::from(count);
    let mean = sum / n;
    let variance = (sum_sq / n - mean * mean).max(0.0);
    Some(TempStats {
        min,
        max,
        mean: mean as f32,
        std_dev: variance.sqrt() as f32,
    })
}

/// Sample a display coordinate directly from a 192x256 f32 plane.
pub fn sample_plane(plane: &[f32], display_x: f32, display_y: f32) -> Option<f32> {
    if plane.len() != TEMP_PLANE_SAMPLES {
        return None;
    }
    let (px, py) = plane_coords(display_x, display_y)?;
    plane.get(py * TEMP_PLANE_W + px).copied()
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes.starts_with(&[0xFF, 0xD8]) && bytes.ends_with(&[0xFF, 0xD9])
}

/// Map a display coordinate to the measurement plane (§6.1, §13.9).
///
/// `plane = display / 1.25`, no rotation, no flip. Returns `None` outside the
/// display.
pub fn plane_coords(display_x: f32, display_y: f32) -> Option<(usize, usize)> {
    if !display_x.is_finite() || !display_y.is_finite() {
        return None;
    }
    if !(0.0..FRAME_W as f32).contains(&display_x) || !(0.0..FRAME_H as f32).contains(&display_y) {
        return None;
    }
    let px = (display_x * DISPLAY_TO_PLANE).floor() as usize;
    let py = (display_y * DISPLAY_TO_PLANE).floor() as usize;
    Some((px.min(TEMP_PLANE_W - 1), py.min(TEMP_PLANE_H - 1)))
}

/// Pearson correlation between a 240x320 luma plane and the temperature plane
/// sampled through the documented display mapping (§13.9).
///
/// The reference capture scores +0.828 here, versus +0.531 for the next-best
/// orientation. Rendering the plane 256 wide instead of 192 tiles it into bands
/// and drops the correlation sharply.
pub fn luma_correlation(plane: &[f32], luma: &[f32]) -> Option<f32> {
    if plane.len() != TEMP_PLANE_SAMPLES || luma.len() != FRAME_W * FRAME_H {
        return None;
    }

    let mut pairs = Vec::with_capacity(luma.len());
    for y in 0..FRAME_H {
        for x in 0..FRAME_W {
            let (px, py) = plane_coords(x as f32, y as f32)?;
            pairs.push((
                f64::from(plane[py * TEMP_PLANE_W + px]),
                f64::from(luma[y * FRAME_W + x]),
            ));
        }
    }
    pearson(&pairs)
}

fn pearson(pairs: &[(f64, f64)]) -> Option<f32> {
    let n = pairs.len() as f64;
    if pairs.len() < 2 {
        return None;
    }
    let mean_a = pairs.iter().map(|(a, _)| a).sum::<f64>() / n;
    let mean_b = pairs.iter().map(|(_, b)| b).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    for (a, b) in pairs {
        let da = a - mean_a;
        let db = b - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }
    if var_a <= f64::EPSILON || var_b <= f64::EPSILON {
        return None;
    }
    Some((cov / (var_a.sqrt() * var_b.sqrt())) as f32)
}

// ---------------------------------------------------------------------------
// Offline verification against a stored capture (§13.9)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct VerifyReport {
    pub shot_jpeg_bytes: usize,
    pub vis_jpeg_bytes: usize,
    pub temperature_samples: usize,
    pub y16_bytes: usize,
    pub stats: TempStats,
    pub correlation: f32,
}

/// Load a stored `PhotoCap` reply, framed or as a bare payload dump.
pub fn load_capture_file(path: &Path) -> std::result::Result<Capture, String> {
    let bytes = fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let payload = match protocol::read_message(&mut Cursor::new(&bytes), None) {
        Ok(message) => {
            if message.token != TOKEN_PHOTOCAP {
                return Err(format!(
                    "{}: expected token {TOKEN_PHOTOCAP:?}, found {:?}",
                    path.display(),
                    message.token
                ));
            }
            message.payload
        }
        Err(_) => bytes,
    };
    Capture::parse(&payload).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse a stored `PhotoCap` reply (framed or bare payload) and compute the
/// documented luma correlation against the OSD screenshot.
pub fn verify_capture_file(path: &Path) -> std::result::Result<VerifyReport, String> {
    let capture = load_capture_file(path)?;
    let (luma, width, height) = decode_luma(&capture.shot_jpeg)?;
    if (width, height) != (FRAME_W, FRAME_H) {
        return Err(format!(
            "OSD JPEG is {width}x{height}, expected {FRAME_W}x{FRAME_H}"
        ));
    }
    let correlation = luma_correlation(&capture.temperature_c, &luma)
        .ok_or_else(|| "cannot correlate: empty or constant plane".to_owned())?;

    Ok(VerifyReport {
        shot_jpeg_bytes: capture.shot_jpeg.len(),
        vis_jpeg_bytes: capture.vis_jpeg.len(),
        temperature_samples: capture.temperature_c.len(),
        y16_bytes: capture.y16.len(),
        stats: capture.stats(),
        correlation,
    })
}

fn decode_luma(jpeg: &[u8]) -> std::result::Result<(Vec<f32>, usize, usize), String> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
    let pixels = decoder
        .decode()
        .map_err(|e| format!("JPEG decode failed: {e}"))?;
    let info = decoder
        .info()
        .ok_or("JPEG decoder returned no image info")?;
    let width = usize::from(info.width);
    let height = usize::from(info.height);

    let luma: Vec<f32> = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => pixels.iter().map(|&v| f32::from(v)).collect(),
        jpeg_decoder::PixelFormat::RGB24 => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| 0.299 * f32::from(p[0]) + 0.587 * f32::from(p[1]) + 0.114 * f32::from(p[2]))
            .collect(),
        other => return Err(format!("unsupported JPEG pixel format {other:?}")),
    };

    if luma.len() != width * height {
        return Err(format!(
            "decoded {} luma samples, expected {}",
            luma.len(),
            width * height
        ));
    }
    Ok((luma, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_plane() -> Vec<f32> {
        let mut plane = Vec::with_capacity(TEMP_PLANE_SAMPLES);
        for y in 0..TEMP_PLANE_H {
            for x in 0..TEMP_PLANE_W {
                let fx = x as f32 / TEMP_PLANE_W as f32;
                let fy = y as f32 / TEMP_PLANE_H as f32;
                plane.push(20.0 + 8.0 * fx + 4.0 * fy + 0.5 * (fx * 12.0).sin());
            }
        }
        plane
    }

    fn sample_to_display_luma(plane: &[f32]) -> Vec<f32> {
        let mut luma = Vec::with_capacity(FRAME_W * FRAME_H);
        for y in 0..FRAME_H {
            for x in 0..FRAME_W {
                let (px, py) = plane_coords(x as f32, y as f32).expect("inside display");
                luma.push(plane[py * TEMP_PLANE_W + px]);
            }
        }
        luma
    }

    #[test]
    fn plane_mapping_is_125_and_unflipped() {
        // Corners of the display map to the four corners of the plane in the same
        // order (no rotation, no flip).
        assert_eq!(plane_coords(0.0, 0.0), Some((0, 0)));
        assert_eq!(plane_coords(239.0, 319.0), Some((191, 255)));
        assert_eq!(plane_coords(120.0, 160.0), Some((96, 128)));
        assert_eq!(plane_coords(240.0, 10.0), None);
        assert_eq!(plane_coords(10.0, -1.0), None);
    }

    #[test]
    fn correlation_is_one_for_matching_orientation() {
        let plane = synthetic_plane();
        let luma = sample_to_display_luma(&plane);
        let correlation = luma_correlation(&plane, &luma).expect("correlation");
        assert!(
            (correlation - 1.0).abs() < 1e-3,
            "correlation {correlation}"
        );
    }

    #[test]
    fn correlation_drops_with_wrong_stride() {
        // Emulate the 256-wide misreading (tiled bands) and require that the
        // documented 192 stride remains clearly better.
        let plane = synthetic_plane();
        let luma = sample_to_display_luma(&plane);

        let mut wrong = Vec::with_capacity(luma.len());
        for y in 0..FRAME_H {
            for x in 0..FRAME_W {
                let px = (x as f32 * DISPLAY_TO_PLANE) as usize;
                let py = (y as f32 * DISPLAY_TO_PLANE) as usize;
                let index = (py * 256 + px) % TEMP_PLANE_SAMPLES;
                wrong.push(plane[index]);
            }
        }

        let right = luma_correlation(&plane, &luma).expect("right stride");
        let bad = luma_correlation(&plane, &wrong).expect("wrong stride");
        assert!(right > 0.99, "right stride {right}");
        assert!(bad < 0.8, "wrong stride should decorrelate, got {bad}");
    }

    #[test]
    fn parses_a_synthetic_capture_and_reports_stats() {
        let message = crate::fixtures::photocap_message();
        let mut cursor = Cursor::new(message);
        let framed = protocol::read_message(&mut cursor, Some(TOKEN_PHOTOCAP)).expect("parse");
        assert_eq!(framed.token, TOKEN_PHOTOCAP);
        let capture = Capture::parse(&framed.payload).expect("capture");
        assert_eq!(capture.temperature_c.len(), TEMP_PLANE_SAMPLES);
        assert_eq!(capture.y16.len(), Y16_PLANE_BYTES);
        assert_eq!(capture.tail, [0u8; 8]);
        assert!(is_jpeg(&capture.shot_jpeg));
        assert!(is_jpeg(&capture.vis_jpeg));

        let stats = capture.stats();
        assert!(stats.min < stats.max);
        assert!((stats.mean - crate::fixtures::SAMPLE_PLANE_MEAN).abs() < 0.5);
        assert_eq!(capture.temperature_at_plane(0, 0), Some(31.0));
        assert_eq!(
            capture.temperature_at_display(0.0, 0.0),
            capture.temperature_at_plane(0, 0)
        );
    }

    #[test]
    fn capture_roundtrips_through_payload() {
        let message = crate::fixtures::photocap_message();
        let capture = Capture::parse(&crate::fixtures::photocap_payload()).expect("fixture");
        assert_eq!(capture.to_message(), message);
        let reparsed = Capture::parse(&capture.to_payload()).expect("payload roundtrip");
        assert_eq!(reparsed.temperature_c, capture.temperature_c);
        assert_eq!(reparsed.shot_jpeg, capture.shot_jpeg);
    }

    #[test]
    fn rejects_short_temperature_plane() {
        let mut payload = crate::fixtures::photocap_payload();
        // Shrink section 1's length word and remove one sample.
        let first = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as usize;
        let temp_len_off = 4 + first;
        let temp_len =
            u32::from_be_bytes(payload[temp_len_off..temp_len_off + 4].try_into().unwrap())
                as usize;
        payload[temp_len_off..temp_len_off + 4]
            .copy_from_slice(&((temp_len - 4) as u32).to_be_bytes());
        payload.drain(temp_len_off + 4 + temp_len - 4..temp_len_off + 4 + temp_len);
        let err = Capture::parse(&payload).unwrap_err();
        assert!(
            matches!(
                err,
                ProtoError::BadPayloadLen {
                    what: "PhotoCap temperature plane",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut payload = crate::fixtures::photocap_payload();
        payload.push(0);
        let err = Capture::parse(&payload).unwrap_err();
        assert!(matches!(err, ProtoError::Field { .. }), "{err}");
    }

    #[test]
    fn rejects_non_jpeg_screenshot() {
        let mut payload = crate::fixtures::photocap_payload();
        payload[4] = 0; // first section body starts after its length word
        let err = Capture::parse(&payload).unwrap_err();
        assert!(matches!(err, ProtoError::Field { .. }), "{err}");
    }
}
