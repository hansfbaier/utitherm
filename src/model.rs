//! Model/geometry table for the UTi_cmdsys family (§1.1, §5.2, §7).
//!
//! Live decoding is length-driven: the payload length selects a pixel format,
//! display geometry and optional temperature-plane size. Geometry becomes
//! authoritative once a `PhotoCap` reply has been decoded.

use std::fmt;

/// Pixel encoding used by the live `vFrame` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Planar Y, U, V; 4:2:0.
    I420,
    /// Interleaved R, G, B; no padding.
    Rgb888,
}

impl PixelFormat {
    pub fn is_i420(self) -> bool {
        matches!(self, Self::I420)
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I420 => f.write_str("I420"),
            Self::Rgb888 => f.write_str("RGB888"),
        }
    }
}

/// Display and raw sensor geometry for one family member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub display_w: usize,
    pub display_h: usize,
    pub raw_w: usize,
    pub raw_h: usize,
    pub format: PixelFormat,
}

impl Geometry {
    pub const fn new(
        display_w: usize,
        display_h: usize,
        raw_w: usize,
        raw_h: usize,
        format: PixelFormat,
    ) -> Self {
        Self {
            display_w,
            display_h,
            raw_w,
            raw_h,
            format,
        }
    }

    /// Bytes in the live pixel block (without key temperatures or tail).
    pub fn pixel_len(self) -> usize {
        match self.format {
            PixelFormat::I420 => self.display_w * self.display_h * 3 / 2,
            PixelFormat::Rgb888 => self.display_w * self.display_h * 3,
        }
    }

    pub fn raw_samples(self) -> usize {
        self.raw_w * self.raw_h
    }

    pub fn f32_plane_len(self) -> usize {
        self.raw_samples() * 4
    }

    pub fn u16_plane_len(self) -> usize {
        self.raw_samples() * 2
    }

    pub fn display_len(self) -> usize {
        self.display_w * self.display_h
    }

    pub fn rgba_len(self) -> usize {
        self.display_len() * 4
    }

    pub fn label(self) -> String {
        format!(
            "{}x{} display, {}x{} raw, {}",
            self.display_w, self.display_h, self.raw_w, self.raw_h, self.format
        )
    }
}

/// The hardware-verified reference model: UTi260B+ / UTi720B.
pub const REFERENCE: Geometry = Geometry::new(240, 320, 192, 256, PixelFormat::I420);

/// Known Tier 1 and Tier 2 geometries from the family spec (§1.1).
///
/// Duplicate geometries are intentional only when a model is a SKU alias; the
/// table is deduplicated below for matching.
pub const GEOMETRIES: &[Geometry] = &[
    REFERENCE,                                              // UTi260B+, UTi720B
    Geometry::new(240, 320, 120, 160, PixelFormat::Rgb888), // UTi165B+/UTi716B
    Geometry::new(180, 240, 120, 160, PixelFormat::Rgb888), // 180x240 variant
    Geometry::new(640, 480, 320, 240, PixelFormat::Rgb888), // UTi320V+/UTi320E
    Geometry::new(640, 480, 256, 192, PixelFormat::Rgb888), // UTi260V/E/T, UTi720T
    Geometry::new(640, 480, 260, 195, PixelFormat::Rgb888), // older UTi260E variant
    Geometry::new(640, 480, 384, 288, PixelFormat::Rgb888), // UTi384H family
];

/// Optional live-plane kind selected by tail size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    None,
    U16,
    F32,
}

/// A length-arithmetic match for one live payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameGeometry {
    pub geometry: Geometry,
    pub plane: PlaneKind,
}

/// Try every known geometry against a live payload length.
///
/// Some 640x480 base frames are ambiguous until a plane arrives or a
/// `PhotoCap` fixes the raw grid; callers should keep the newest match and let
/// later frames/`PhotoCap` refine it.
pub fn frame_geometries(payload_len: usize) -> Vec<FrameGeometry> {
    let mut out = Vec::new();
    for &geometry in GEOMETRIES {
        let base = geometry.pixel_len() + 12;
        if payload_len == base {
            push_unique(
                &mut out,
                FrameGeometry {
                    geometry,
                    plane: PlaneKind::None,
                },
            );
        }
        if payload_len == base + geometry.f32_plane_len() {
            push_unique(
                &mut out,
                FrameGeometry {
                    geometry,
                    plane: PlaneKind::F32,
                },
            );
        }
        if payload_len == base + geometry.u16_plane_len() {
            push_unique(
                &mut out,
                FrameGeometry {
                    geometry,
                    plane: PlaneKind::U16,
                },
            );
        }
    }
    out
}

fn push_unique(out: &mut Vec<FrameGeometry>, item: FrameGeometry) {
    if !out
        .iter()
        .any(|existing| existing.geometry == item.geometry && existing.plane == item.plane)
    {
        out.push(item);
    }
}

/// Default geometry before any frame or `PhotoCap` has been seen.
pub const fn default_geometry() -> Geometry {
    REFERENCE
}

/// Derive raw width/height from a plane area and the display aspect ratio.
///
/// Used when `PhotoCap` section 1 gives `raw_w*raw_h` and the JPEG gives the
/// display size. Exact table matches win first; the aspect-ratio solve covers
/// firmware variants.
pub fn derive_raw(display_w: usize, display_h: usize, samples: usize) -> Option<(usize, usize)> {
    if samples == 0 || display_w == 0 || display_h == 0 {
        return None;
    }
    if let Some(geometry) = GEOMETRIES.iter().find(|geometry| {
        geometry.display_w == display_w
            && geometry.display_h == display_h
            && geometry.raw_samples() == samples
    }) {
        return Some((geometry.raw_w, geometry.raw_h));
    }

    // raw_w / raw_h == display_w / display_h (within rounding), so
    // raw_h^2 = samples * display_h / display_w.
    let estimate = (samples as f64 * display_h as f64 / display_w as f64).sqrt();
    for delta in 0..=2i64 {
        for sign in [-1i64, 1] {
            let h = estimate.round() as i64 + sign * delta;
            if h <= 0 {
                continue;
            }
            let w = samples as i64 / h;
            if w > 0 && w * h == samples as i64 && w <= h {
                return Some((w as usize, h as usize));
            }
        }
    }
    None
}

/// Find the table geometry for a known display/raw pair.
pub fn find_geometry(
    display_w: usize,
    display_h: usize,
    raw_w: usize,
    raw_h: usize,
) -> Option<Geometry> {
    GEOMETRIES.iter().copied().find(|geometry| {
        geometry.display_w == display_w
            && geometry.display_h == display_h
            && geometry.raw_w == raw_w
            && geometry.raw_h == raw_h
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_and_i420_lengths_match_live_fixtures() {
        let base = REFERENCE.pixel_len() + 12;
        assert_eq!(base, 115_212);
        assert_eq!(frame_geometries(base)[0].geometry, REFERENCE);
        let extended = base + REFERENCE.f32_plane_len();
        assert_eq!(extended, 311_820);
        let matches = frame_geometries(extended);
        assert!(
            matches
                .iter()
                .any(|m| { m.geometry == REFERENCE && m.plane == PlaneKind::F32 })
        );
    }

    #[test]
    fn every_table_geometry_round_trips_through_length_arithmetic() {
        for &geometry in GEOMETRIES {
            let base = geometry.pixel_len() + 12;
            assert!(
                frame_geometries(base)
                    .iter()
                    .any(|m| { m.geometry == geometry && m.plane == PlaneKind::None }),
                "base {geometry:?}"
            );
            assert!(
                frame_geometries(base + geometry.u16_plane_len())
                    .iter()
                    .any(|m| m.geometry == geometry && m.plane == PlaneKind::U16),
                "u16 tail {geometry:?}"
            );
            assert!(
                frame_geometries(base + geometry.f32_plane_len())
                    .iter()
                    .any(|m| m.geometry == geometry && m.plane == PlaneKind::F32),
                "f32 tail {geometry:?}"
            );
        }
    }

    #[test]
    fn rgb888_tier1_geometry_is_length_selected() {
        let geometry = Geometry::new(640, 480, 256, 192, PixelFormat::Rgb888);
        let base = geometry.pixel_len() + 12;
        assert_eq!(base, 921_612);
        assert!(
            frame_geometries(base)
                .iter()
                .any(|m| m.geometry == geometry && m.plane == PlaneKind::None)
        );
        let extended = base + geometry.f32_plane_len();
        assert!(
            frame_geometries(extended)
                .iter()
                .any(|m| m.geometry == geometry && m.plane == PlaneKind::F32)
        );
    }

    #[test]
    fn raw_area_is_derived_from_display_aspect() {
        assert_eq!(derive_raw(240, 320, 192 * 256), Some((192, 256)));
        assert_eq!(derive_raw(640, 480, 320 * 240), Some((320, 240)));
        assert_eq!(derive_raw(640, 480, 256 * 192), Some((256, 192)));
        assert_eq!(derive_raw(640, 480, 384 * 288), Some((384, 288)));
        assert_eq!(derive_raw(640, 480, 260 * 195), Some((260, 195)));
    }
}
