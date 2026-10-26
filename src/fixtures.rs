//! Synthetic fixtures.
//!
//! The spec (§14) lists the `init` reply and full `vFrame` message as missing
//! captures. Until a device capture exists, these builders produce byte-exact
//! synthetic messages using the documented values so the framing, parser and
//! replay path can be regression-tested without a camera.
//!
//! `--make-fixtures <dir>` writes them to disk for `--replay`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::capture;
use crate::protocol::{
    self, FRAME_H, FRAME_W, INIT_PAYLOAD_LEN, KEY_TEMP_LEN, TOKEN_INIT, TOKEN_PHOTOCAP,
    TOKEN_VFRAME, VFRAME_PAYLOAD_LEN, YUV_CHROMA_LEN, YUV_LEN, YUV_Y_LEN,
};

fn put_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(buf: &mut [u8], off: usize, value: i32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_f32(buf: &mut [u8], off: usize, value: f32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

/// `init` payload with the values observed on firmware 1.1.9 (§4 table).
pub fn init_payload() -> Vec<u8> {
    let mut p = vec![0u8; INIT_PAYLOAD_LEN];

    put_u32(&mut p, 0, 0x0f); // mark/menu flags: all four enabled
    put_u32(&mut p, 4, 0); // IR only
    put_i32(&mut p, 8, 60); // pip rect
    put_i32(&mut p, 12, 80);
    put_i32(&mut p, 16, 120);
    put_i32(&mut p, 20, 160);
    put_f32(&mut p, 24, 0.25); // distance, m
    put_f32(&mut p, 28, 0.5); // fusion level

    // Colour-scale block, observed bytes at +8/+12; semantics unknown [U].
    p[32 + 8..32 + 16].copy_from_slice(&[0x6e, 0x56, 0x4b, 0x42, 0x4d, 0xf8, 0xfe, 0x41]);

    put_f32(&mut p, 56, 0.95); // emissivity
    put_f32(&mut p, 60, 25.0); // ambient / reflected
    put_f32(&mut p, 64, 25.0); // second ambient field [U]
    put_u32(&mut p, 68, 55); // humidity %RH
    put_u32(&mut p, 72, 25); // ambient integer
    put_u32(&mut p, 76, 0); // alarm enable
    put_f32(&mut p, 80, 5.0); // alarm high delta, K
    put_f32(&mut p, 84, 36.7); // alarm value, °C
    put_u32(&mut p, 88, 0); // °C
    put_u32(&mut p, 92, 1); // camera info [U]
    put_u32(&mut p, 96, 20);
    put_u32(&mut p, 100, 1); // other info [U]
    put_u32(&mut p, 104, 70);
    p[108..110].copy_from_slice(&160i16.to_le_bytes()); // laser pixel [U]
    p[110..112].copy_from_slice(&229i16.to_le_bytes());
    // fuse XY (112..124) stays zero [U].

    // Version block at 124: packed layout observed on the device.
    p[124..127].copy_from_slice(b"1.0");
    p[128..132].copy_from_slice(b"T2.0");
    p[136..141].copy_from_slice(b"1.1.9");

    p
}

/// Full 160-byte `init` reply message.
pub fn init_message() -> Vec<u8> {
    protocol::encode_message(TOKEN_INIT, &init_payload())
}

/// Parse the synthetic init payload back out (tests only).
#[cfg(test)]
pub fn init_reply() -> protocol::InitReply {
    protocol::InitReply::parse(&init_payload()).expect("fixture init payload is well formed")
}

/// A 240x320/4:2:0 test pattern: luma ramp plus chroma quadrants so the BT.601
/// conversion exercises every plane.
pub fn yuv_frame() -> Vec<u8> {
    let mut y_plane = vec![0u8; YUV_Y_LEN];
    for y in 0..FRAME_H {
        for x in 0..FRAME_W {
            // Diagonal ramp kept inside the full-range window.
            let v = 16 + ((x * 2 + y) % 220) as u8;
            y_plane[y * FRAME_W + x] = v;
        }
    }

    let cw = FRAME_W / 2;
    let ch = FRAME_H / 2;
    let mut u_plane = vec![128u8; YUV_CHROMA_LEN];
    let mut v_plane = vec![128u8; YUV_CHROMA_LEN];
    for cy in 0..ch {
        for cx in 0..cw {
            let idx = cy * cw + cx;
            u_plane[idx] = if cx < cw / 2 { 96 } else { 176 };
            v_plane[idx] = if cy < ch / 2 { 96 } else { 176 };
        }
    }

    let mut out = Vec::with_capacity(YUV_LEN);
    out.extend_from_slice(&y_plane);
    out.extend_from_slice(&u_plane);
    out.extend_from_slice(&v_plane);
    out
}

/// Key temperatures from the captured frame in §5.2 (max/min/centre, ordering
/// still unverified). These are the exact big-endian bytes of the sample:
/// `42 0e b1 ae | 41 f8 ab cd | 41 fd eb 9d` (spec prints them rounded).
pub const SAMPLE_KEY_BYTES: [u8; KEY_TEMP_LEN] = [
    0x42, 0x0e, 0xb1, 0xae, 0x41, 0xf8, 0xab, 0xcd, 0x41, 0xfd, 0xeb, 0x9d,
];

#[cfg(test)]
pub const SAMPLE_KEY_C: [f32; 3] = [
    f32::from_be_bytes([0x42, 0x0e, 0xb1, 0xae]),
    f32::from_be_bytes([0x41, 0xf8, 0xab, 0xcd]),
    f32::from_be_bytes([0x41, 0xfd, 0xeb, 0x9d]),
];

/// Full 115 227-byte `vFrame` reply message with the captured key-temp bytes.
pub fn vframe_message() -> Vec<u8> {
    let mut payload = Vec::with_capacity(VFRAME_PAYLOAD_LEN);
    payload.extend_from_slice(&yuv_frame());
    payload.extend_from_slice(&SAMPLE_KEY_BYTES);
    protocol::encode_message(TOKEN_VFRAME, &payload)
}

/// Build a `vFrame` message from arbitrary pixel/key data (tests only).
#[cfg(test)]
pub fn vframe_message_with(yuv: &[u8], key_c: [f32; 3]) -> Vec<u8> {
    assert_eq!(yuv.len(), YUV_LEN, "YUV frame must be {YUV_LEN} bytes");
    let mut payload = Vec::with_capacity(VFRAME_PAYLOAD_LEN);
    payload.extend_from_slice(yuv);
    for k in key_c {
        payload.extend_from_slice(&k.to_be_bytes());
    }
    assert_eq!(payload.len(), YUV_LEN + KEY_TEMP_LEN);
    protocol::encode_message(TOKEN_VFRAME, &payload)
}

/// Mean of the synthetic temperature plane used by [`photocap_payload`].
#[cfg(test)]
pub const SAMPLE_PLANE_MEAN: f32 = 32.0;

fn fake_jpeg(len: usize) -> Vec<u8> {
    assert!(len >= 4, "JPEG needs at least SOI and EOI");
    let mut jpeg = vec![0u8; len];
    jpeg[0] = 0xFF;
    jpeg[1] = 0xD8;
    jpeg[len - 2] = 0xFF;
    jpeg[len - 1] = 0xD9;
    jpeg
}

/// Synthetic temperature plane, f32 LE: 31.0 .. 33.0 °C, mean 32.0.
pub fn sample_plane() -> Vec<f32> {
    let samples = capture::TEMP_PLANE_SAMPLES;
    let mut plane = Vec::with_capacity(samples);
    for index in 0..samples {
        // Exactly 31.0 at index 0 and 33.0 at the last sample.
        plane.push(31.0 + 2.0 * index as f32 / (samples - 1) as f32);
    }
    plane
}

/// Synthetic live temperature plane, f32 LE, 192x256.
pub fn sample_plane_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(crate::capture::TEMP_PLANE_BYTES);
    for value in sample_plane() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Extended `vFrame` reply as the device sends on every 11th request:
/// YUV + key temps + live 192x256 f32 plane.
#[cfg(test)]
pub fn vframe_extended_message() -> Vec<u8> {
    let plane_bytes = sample_plane_bytes();
    let mut payload = Vec::with_capacity(VFRAME_PAYLOAD_LEN + plane_bytes.len());
    payload.extend_from_slice(&yuv_frame());
    payload.extend_from_slice(&SAMPLE_KEY_BYTES);
    payload.extend_from_slice(&plane_bytes);
    assert_eq!(
        payload.len(),
        VFRAME_PAYLOAD_LEN + crate::protocol::LIVE_PLANE_BYTES
    );
    protocol::encode_message(TOKEN_VFRAME, &payload)
}

/// Bare `PhotoCap` payload with the documented section geometry.
///
/// Sections 0 and 2 are placeholder JPEG marker wrappers, not decodable images:
/// enough to exercise framing and replay, not to run the luma self-test.
pub fn photocap_payload() -> Vec<u8> {
    let plane_bytes = sample_plane_bytes();

    // JPEG stubs have the observed sizes; only the marker wrappers are valid.
    let shot = fake_jpeg(54_057);
    let vis = fake_jpeg(214_647);
    let y16 = vec![0u8; capture::Y16_PLANE_BYTES];
    let tail = [0u8; 8];
    let bodies: [&[u8]; 5] = [&shot, &plane_bytes, &vis, &y16, &tail];

    let mut payload = Vec::new();
    for body in bodies {
        payload.extend_from_slice(&(body.len() as u32).to_be_bytes());
        payload.extend_from_slice(body);
    }
    payload
}

/// Full framed `PhotoCap` reply.
pub fn photocap_message() -> Vec<u8> {
    protocol::encode_message(TOKEN_PHOTOCAP, &photocap_payload())
}

/// Write the synthetic fixture set to `dir` and return the paths written.
pub fn write_all(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    fs::create_dir_all(dir)?;
    let mut written = Vec::new();

    let init_path = dir.join("init.bin");
    fs::write(&init_path, init_message())?;
    written.push(init_path);

    let vframe_path = dir.join("vframe.bin");
    fs::write(&vframe_path, vframe_message())?;
    written.push(vframe_path);

    let yuv_path = dir.join("live_clean.yuv");
    fs::write(&yuv_path, yuv_frame())?;
    written.push(yuv_path);

    let photocap_path = dir.join("photocap.bin");
    fs::write(&photocap_path, photocap_message())?;
    written.push(photocap_path);

    let readme = dir.join("README.txt");
    if !readme.exists() {
        let mut f = fs::File::create(&readme)?;
        writeln!(
            f,
            "Synthetic UTi260B+ fixtures (documented values, not device captures).\n\
             init.bin         160 B framed UTi_cmdsys:init reply\n\
             vframe.bin       115 227 B framed UTi_cmdsys:vFrame reply\n\
             live_clean.yuv   115 200 B raw YUV 4:2:0 pixel block (240x320)\n\
             photocap.bin     563 661 B framed PhotoCap reply (JPEG stubs, synthetic plane)\n\
             \n\
             Replay with: uti260b-viewer --replay {}",
            dir.display()
        )?;
        written.push(readme);
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{self, TOKEN_VFRAME};

    #[test]
    fn fixture_sizes() {
        assert_eq!(init_message().len(), 160);
        assert_eq!(vframe_message().len(), 115_227);
        assert_eq!(yuv_frame().len(), 115_200);
        assert_eq!(photocap_message().len(), 563_661);
        assert_eq!(vframe_extended_message().len(), 311_835);
        assert_eq!(
            sample_plane_bytes().len() / 4,
            crate::protocol::LIVE_PLANE_SAMPLES
        );
    }

    #[test]
    fn yuv_frame_converts_without_panic() {
        let yuv = yuv_frame();
        let mut rgba = Vec::new();
        crate::yuv::yuv420_to_rgba(&yuv, &mut rgba);
        assert_eq!(rgba.len(), FRAME_W * FRAME_H * 4);
        assert!(rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn custom_key_message_roundtrips() {
        let keys = [1.5f32, -2.25, 99.0];
        let bytes = vframe_message_with(&yuv_frame(), keys);
        let mut cur = std::io::Cursor::new(bytes);
        let msg = protocol::read_message(&mut cur, Some(TOKEN_VFRAME)).expect("parse");
        let frame = protocol::parse_vframe(&msg.payload).expect("payload");
        assert_eq!(frame.key_c, keys);
    }

    #[test]
    fn vframe_message_parses_back() {
        let bytes = vframe_message();
        let mut cur = std::io::Cursor::new(bytes);
        let msg = protocol::read_message(&mut cur, Some(TOKEN_VFRAME)).expect("parse");
        let frame = protocol::parse_vframe(&msg.payload).expect("payload");
        assert_eq!(frame.key_c, SAMPLE_KEY_C);
        assert_eq!(&msg.payload[YUV_LEN..], &SAMPLE_KEY_BYTES);
    }
}
