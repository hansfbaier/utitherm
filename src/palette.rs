//! Palette signature helpers (spec §3.1).
//!
//! The device's palette indices are not mapped to names by the protocol; these
//! helpers only compare a decoded frame against the measured reference
//! signatures so a requested change can be confirmed or flagged.

use crate::protocol::{PALETTE_SIGNATURES, Palette};

/// Euclidean distance in (mean R, G, B, saturation) still counted as a match.
/// Deliberately loose: the reference table is one scene including the burned-in
/// OSD, not a palette fingerprint (§3.1).
pub const PALETTE_MATCH_DISTANCE: f32 = 45.0;

/// Mean R, G, B and mean saturation (max-min channel) of a decoded frame.
pub fn frame_signature(rgba: &[u8]) -> (f32, f32, f32, f32) {
    let mut sum_r = 0.0f64;
    let mut sum_g = 0.0f64;
    let mut sum_b = 0.0f64;
    let mut sum_sat = 0.0f64;
    let pixels = rgba.len() / 4;
    if pixels == 0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    for pixel in rgba.as_chunks::<4>().0 {
        let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
        sum_r += f64::from(r);
        sum_g += f64::from(g);
        sum_b += f64::from(b);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        sum_sat += f64::from(max - min);
    }
    let n = pixels as f64;
    (
        (sum_r / n) as f32,
        (sum_g / n) as f32,
        (sum_b / n) as f32,
        (sum_sat / n) as f32,
    )
}

/// Nearest reference palette signature and its Euclidean distance.
pub fn nearest_palette((r, g, b, sat): (f32, f32, f32, f32)) -> (Palette, f32) {
    let mut best = (Palette::P0, f32::INFINITY);
    for (index, signature) in PALETTE_SIGNATURES.iter().enumerate() {
        let dr = r - signature.0;
        let dg = g - signature.1;
        let db = b - signature.2;
        let ds = sat - signature.3;
        let distance = (dr * dr + dg * dg + db * db + ds * ds).sqrt();
        if distance < best.1 {
            best = (
                Palette::from_index(index as u8).expect("index within whitelist"),
                distance,
            );
        }
    }
    best
}

/// P0 and P1 are nearly identical in the reference data.
pub fn equivalent(requested: Palette, detected: Palette) -> bool {
    requested == detected
        || matches!(
            (requested, detected),
            (Palette::P0, Palette::P1) | (Palette::P1, Palette::P0)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_frame_matches_the_grey_palette() {
        let rgba: Vec<u8> = [136u8, 136, 135, 255]
            .iter()
            .copied()
            .cycle()
            .take(crate::protocol::FRAME_W * crate::protocol::FRAME_H * 4)
            .collect();
        let signature = frame_signature(&rgba);
        let (palette, distance) = nearest_palette(signature);
        assert_eq!(palette, Palette::P3, "signature {signature:?}");
        assert!(distance < 10.0, "distance {distance}");
    }
}
