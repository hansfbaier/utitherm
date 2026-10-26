//! Read-only import of the camera's USB radiometric container (§11).
//!
//! Container layout (all multi-byte fields big endian on the wire):
//!
//! ```text
//! JPEG #1 (240x320, with OSD)
//! u32 BE len2
//! JPEG #2 (480x640, len2 bytes)
//! u32 BE len2 (repeated)
//! 49 152 x { f32 BE temperature °C, u32 BE aux }   // interleaved
//! 4 063-byte metadata block
//! ```
//!
//! The volume itself is treated as strictly read-only. Import copies the file into
//! a host cache and indexes it by SHA-256; nothing on the camera is ever written.

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capture::{TEMP_PLANE_SAMPLES, TempStats, plane_stats};

pub const USB_METADATA_BYTES: usize = 4_063;
pub const USB_RECORD_BYTES: usize = 8;
pub const USB_RECORDS_BYTES: usize = TEMP_PLANE_SAMPLES * USB_RECORD_BYTES; // 393 216
pub const USB_TEMP_MIN_C: f32 = -40.0;
pub const USB_TEMP_MAX_C: f32 = 400.0;
pub const USB_AUTORUN_MARKER: &str = "UNIT UTi260B+";

pub type UsbResult<T> = Result<T, String>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsbMetadata {
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub emissivity: Option<f32>,
    #[serde(default)]
    pub distance_m: Option<f32>,
    #[serde(default)]
    pub humidity_pct: Option<u32>,
    #[serde(default)]
    pub ambient: Option<f32>,
    #[serde(default)]
    pub unit: Option<u32>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub rectangles: Vec<[u32; 4]>,
}

#[derive(Debug, Clone)]
pub struct UsbImage {
    pub source_path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub captured: Option<String>,
    pub jpeg1: Vec<u8>,
    pub jpeg2: Vec<u8>,
    pub temperature_c: Vec<f32>,
    pub aux: Vec<u32>,
    pub metadata: UsbMetadata,
    pub stats: TempStats,
    /// RGBA 240x320 thumbnail if JPEG #1 decoded; `None` for synthetic fixtures.
    pub thumbnail_rgba: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub sha256: String,
    pub source_name: String,
    #[serde(default)]
    pub captured: Option<String>,
    pub cache_file: String,
    #[serde(default)]
    pub thumbnail_file: Option<String>,
    pub metadata: UsbMetadata,
    pub min_c: f32,
    pub max_c: f32,
    pub mean_c: f32,
    pub std_dev_c: f32,
    pub imported_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsbIndex {
    #[serde(default)]
    pub entries: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Clone)]
pub struct Volume {
    pub root: PathBuf,
    pub dcim: PathBuf,
    pub files: Vec<PathBuf>,
}

pub struct ImportOutcome {
    pub entry: IndexEntry,
    pub image: UsbImage,
    pub duplicate: bool,
}

/// Parse one USB container from bytes, without touching the filesystem.
pub fn parse_usb_bytes(source_path: &Path, bytes: &[u8]) -> UsbResult<UsbImage> {
    if bytes.len() < 8 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err("not a radiometric container: missing JPEG #1 SOI".to_owned());
    }

    let jpeg1_end = find_eoi(bytes, 2)?;
    let min_tail = USB_RECORDS_BYTES + USB_METADATA_BYTES + 8;
    if bytes.len() < jpeg1_end + min_tail {
        return Err(format!(
            "truncated container: {} bytes, need at least {} after JPEG #1",
            bytes.len(),
            min_tail
        ));
    }

    // The visible JPEG is optional (§11.4): a thermal-only file stores
    // i32 BE -1 followed by a zero length word, then goes straight to records.
    let len2 = i32::from_be_bytes(
        bytes[jpeg1_end..jpeg1_end + 4]
            .try_into()
            .expect("slice of four"),
    );
    let (jpeg2_start, jpeg2_end, records_start) = if len2 == -1 {
        let absent = read_u32(bytes, jpeg1_end + 4)?;
        if absent != 0 {
            return Err(format!(
                "thermal-only container must have a zero second length word, got {absent}"
            ));
        }
        (jpeg1_end + 4, jpeg1_end + 4, jpeg1_end + 8)
    } else if len2 > 0 {
        let len2 = len2 as usize;
        let jpeg2_start = jpeg1_end + 4;
        let jpeg2_end = jpeg2_start
            .checked_add(len2)
            .ok_or_else(|| "JPEG #2 length overflows".to_owned())?;
        if jpeg2_end + 4 > bytes.len() {
            return Err(format!(
                "JPEG #2 length {len2} exceeds file ({} bytes)",
                bytes.len()
            ));
        }
        let len2_again = read_u32(bytes, jpeg2_end)? as usize;
        if len2 != len2_again {
            return Err(format!(
                "length words disagree: {len2} before JPEG #2, {len2_again} after"
            ));
        }
        if len2 < 4
            || bytes[jpeg2_start..jpeg2_start + 2] != [0xFF, 0xD8]
            || bytes[jpeg2_end - 2..jpeg2_end] != [0xFF, 0xD9]
        {
            return Err("JPEG #2 is not bounded by SOI/EOI".to_owned());
        }
        (jpeg2_start, jpeg2_end, jpeg2_end + 4)
    } else {
        return Err(format!("invalid JPEG #2 length {len2}"));
    };
    let metadata_start = records_start
        .checked_add(USB_RECORDS_BYTES)
        .ok_or_else(|| "record block overflows".to_owned())?;
    let expected_end = metadata_start + USB_METADATA_BYTES;
    if expected_end != bytes.len() {
        return Err(format!(
            "container size equation failed: expected {expected_end}, file is {}",
            bytes.len()
        ));
    }

    let mut temperature_c = Vec::with_capacity(TEMP_PLANE_SAMPLES);
    let mut aux = Vec::with_capacity(TEMP_PLANE_SAMPLES);
    for index in 0..TEMP_PLANE_SAMPLES {
        let offset = records_start + index * USB_RECORD_BYTES;
        let value =
            f32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("slice of four"));
        if !value.is_finite() || !(USB_TEMP_MIN_C..=USB_TEMP_MAX_C).contains(&value) {
            return Err(format!(
                "sample {index} is {value} °C, outside the physical band \
                 {USB_TEMP_MIN_C}..{USB_TEMP_MAX_C}; likely interleaving error"
            ));
        }
        temperature_c.push(value);
        aux.push(read_u32(bytes, offset + 4)?);
    }

    let stats = plane_stats(&temperature_c)
        .ok_or_else(|| "temperature plane is empty or non-finite".to_owned())?;
    let metadata = parse_metadata(&bytes[metadata_start..metadata_start + USB_METADATA_BYTES]);
    let thumbnail_rgba = decode_thumbnail(&bytes[..jpeg1_end]);

    let file_name = source_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "capture.jpg".to_owned());

    Ok(UsbImage {
        source_path: source_path.to_path_buf(),
        captured: captured_from_name(&file_name),
        file_name,
        sha256: sha256_hex(bytes),
        jpeg1: bytes[..jpeg1_end].to_vec(),
        jpeg2: bytes[jpeg2_start..jpeg2_end].to_vec(),
        temperature_c,
        aux,
        metadata,
        stats,
        thumbnail_rgba,
    })
}

pub fn parse_usb_file(path: &Path) -> UsbResult<UsbImage> {
    let bytes = fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    parse_usb_bytes(path, &bytes)
}

/// Import a file into the host cache and index, deduplicating by content hash.
pub fn import_file(path: &Path, cache: &Path, index: &mut UsbIndex) -> UsbResult<ImportOutcome> {
    let bytes = fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let image = parse_usb_bytes(path, &bytes)?;
    let sha = image.sha256.clone();

    if let Some(existing) = index.entries.get(&sha).cloned() {
        let mut entry = existing;
        // A parser improvement may change metadata for an already imported file.
        entry.metadata = image.metadata.clone();
        entry.min_c = image.stats.min;
        entry.max_c = image.stats.max;
        entry.mean_c = image.stats.mean;
        entry.std_dev_c = image.stats.std_dev;
        index.entries.insert(sha, entry.clone());
        save_index(cache, index)?;
        return Ok(ImportOutcome {
            entry,
            image,
            duplicate: true,
        });
    }

    fs::create_dir_all(cache)
        .map_err(|err| format!("cannot create cache {}: {err}", cache.display()))?;
    let cache_name = format!("{sha}.jpg");
    fs::write(cache.join(&cache_name), &bytes)
        .map_err(|err| format!("cannot write cache {}: {err}", cache_name))?;

    let thumbnail_file = match &image.thumbnail_rgba {
        Some(rgba) => {
            let name = format!("{sha}_thumb.png");
            write_png(&cache.join(&name), rgba)?;
            Some(name)
        }
        None => None,
    };

    let entry = IndexEntry {
        sha256: sha.clone(),
        source_name: image.file_name.clone(),
        captured: image.captured.clone(),
        cache_file: cache_name,
        thumbnail_file,
        metadata: image.metadata.clone(),
        min_c: image.stats.min,
        max_c: image.stats.max,
        mean_c: image.stats.mean,
        std_dev_c: image.stats.std_dev,
        imported_at: now_unix(),
    };
    index.entries.insert(sha, entry.clone());
    save_index(cache, index)?;

    Ok(ImportOutcome {
        entry,
        image,
        duplicate: false,
    })
}

pub fn load_index(cache: &Path) -> UsbIndex {
    let path = cache.join("usb-index.json");
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Re-parse cached containers so metadata improvements reach old indexes.
pub fn refresh_index(cache: &Path, index: &mut UsbIndex) -> bool {
    let mut changed = false;
    for entry in index.entries.values_mut() {
        let path = cache.join(&entry.cache_file);
        let Ok(image) = parse_usb_file(&path) else {
            continue;
        };
        if entry.metadata.firmware != image.metadata.firmware
            || entry.metadata.model != image.metadata.model
            || entry.min_c != image.stats.min
            || entry.max_c != image.stats.max
            || entry.mean_c != image.stats.mean
            || entry.std_dev_c != image.stats.std_dev
        {
            entry.metadata = image.metadata;
            entry.min_c = image.stats.min;
            entry.max_c = image.stats.max;
            entry.mean_c = image.stats.mean;
            entry.std_dev_c = image.stats.std_dev;
            changed = true;
        }
    }
    if changed {
        let _ = save_index(cache, index);
    }
    changed
}

pub fn save_index(cache: &Path, index: &UsbIndex) -> UsbResult<()> {
    fs::create_dir_all(cache)
        .map_err(|err| format!("cannot create cache {}: {err}", cache.display()))?;
    let path = cache.join("usb-index.json");
    let tmp = cache.join("usb-index.json.tmp");
    let data = serde_json::to_vec_pretty(index)
        .map_err(|err| format!("cannot serialise USB index: {err}"))?;
    fs::write(&tmp, data).map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|err| format!("cannot replace {}: {err}", path.display()))?;
    Ok(())
}

/// Content-first discovery: a directory is a camera volume when it owns a
/// `DCIM/` directory with JPEGs (or is a `DCIM` directory). An explicit path
/// bypasses the automatic mount scan.
pub fn discover_volumes(explicit: Option<&Path>) -> Vec<Volume> {
    discover_volumes_with_marker(explicit, USB_AUTORUN_MARKER)
}

/// Like [`discover_volumes`], but with a configurable `AUTORUN.INF` marker
/// (§13.7: other models may carry a different label).
pub fn discover_volumes_with_marker(explicit: Option<&Path>, marker: &str) -> Vec<Volume> {
    if let Some(path) = explicit {
        if path.is_file() {
            let dcim = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return vec![Volume {
                root: dcim.clone(),
                dcim,
                files: vec![path.to_path_buf()],
            }];
        }
        return volume_from_path(path).into_iter().collect();
    }

    let mut roots = Vec::new();
    #[cfg(unix)]
    {
        for base in ["/run/media", "/media", "/mnt", "/Volumes"] {
            let base = Path::new(base);
            let Ok(entries) = fs::read_dir(base) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                roots.push(path.clone());
                if let Ok(children) = fs::read_dir(&path) {
                    for child in children.flatten() {
                        if child.path().is_dir() {
                            roots.push(child.path());
                        }
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        for letter in b'D'..=b'Z' {
            let path = PathBuf::from(format!("{}:\\", char::from(letter)));
            if path.is_dir() {
                roots.push(path);
            }
        }
    }

    let mut volumes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            continue;
        }
        if let Some(volume) = volume_from_path(&root)
            && has_camera_autorun(&volume.root, marker)
        {
            volumes.push(volume);
        }
    }
    volumes
}

fn volume_from_path(path: &Path) -> Option<Volume> {
    let is_dcim = path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("DCIM"));
    let (root, dcim) = if is_dcim {
        (path.parent()?.to_path_buf(), path.to_path_buf())
    } else {
        (path.to_path_buf(), path.join("DCIM"))
    };
    if !dcim.is_dir() {
        return None;
    }

    let mut files = list_jpegs(&dcim);
    if files.is_empty() {
        return None;
    }
    files.sort();
    Some(Volume { root, dcim, files })
}

fn list_jpegs(dcim: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dcim) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
        })
        .collect()
}

/// True when the volume root carries the camera's `AUTORUN.INF` marker.
pub fn has_camera_autorun(root: &Path, marker: &str) -> bool {
    fs::read_to_string(root.join("AUTORUN.INF"))
        .map(|text| text.contains(marker))
        .unwrap_or(false)
}

fn find_eoi(bytes: &[u8], from: usize) -> UsbResult<usize> {
    let mut index = from;
    while index + 1 < bytes.len() {
        if bytes[index] == 0xFF && bytes[index + 1] == 0xD9 {
            return Ok(index + 2);
        }
        index += 1;
    }
    Err("JPEG #1 EOI not found".to_owned())
}

fn read_u32(bytes: &[u8], offset: usize) -> UsbResult<u32> {
    bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated u32 at offset {offset}"))?
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| format!("truncated u32 at offset {offset}"))
}

fn parse_metadata(block: &[u8]) -> UsbMetadata {
    // The firmware string starts one byte earlier on some files than the
    // reference sample (3980 vs 3981); accept either position.
    let firmware = ascii_at(block, 3_980, 8)
        .filter(|value| value.matches('.').count() >= 2)
        .or_else(|| ascii_at(block, 3_981, 8).filter(|value| value.matches('.').count() >= 2))
        .unwrap_or_default();
    let model = ascii_at(block, 3_990, 16).unwrap_or_default();
    let mut versions = Vec::new();
    // The version window is not fixed-width: some files store `1.0` and `T2.0`
    // contiguously as `1.0T2.0`, others leave a NUL or a one-byte gap. Split the
    // printable window around the known tokens instead of trusting one offset.
    if let Some(value) = ascii_at(block, 3_970, 12) {
        if value.starts_with("1.0") {
            versions.push("1.0".to_owned());
            let rest = value.strip_prefix("1.0").unwrap_or_default();
            if rest.starts_with("T2.0") {
                versions.push("T2.0".to_owned());
            }
        } else {
            versions.push(value);
        }
    }
    if versions.len() < 2
        && let Some(value) = ascii_at(block, 3_975, 5)
    {
        versions.push(value);
    }
    if !firmware.is_empty() {
        versions.push(firmware.clone());
    }

    let emissivity = ascii_at(block, 4_000, 8).and_then(|text| first_number(&text));
    let distance_m = ascii_at(block, 4_018, 8).and_then(|text| first_number(&text));
    let humidity_pct = ascii_at(block, 4_028, 8)
        .and_then(|text| first_number(&text))
        .map(|value| value as u32);
    let ambient = ascii_at(block, 4_038, 8).and_then(|text| first_number(&text));
    let resolution = ascii_at(block, 4_048, 12);
    let unit = ascii_at(block, 4_058, 4).and_then(|text| text.parse::<u32>().ok());

    let serial = if block.len() >= 4_018 {
        Some(
            block[4_012..4_018]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    } else {
        None
    };

    let mut rectangles = Vec::new();
    for offset in [1_664usize, 1_720] {
        if let Ok(values) = rectangle_at(block, offset)
            && values.iter().any(|value| *value != 0)
        {
            rectangles.push(values);
        }
    }

    UsbMetadata {
        firmware,
        model,
        versions,
        emissivity,
        distance_m,
        humidity_pct,
        ambient,
        unit,
        resolution,
        serial,
        rectangles,
    }
}

fn rectangle_at(block: &[u8], offset: usize) -> UsbResult<[u32; 4]> {
    if offset + 16 > block.len() {
        return Err("rectangle out of range".to_owned());
    }
    Ok([
        read_u32(block, offset)?,
        read_u32(block, offset + 4)?,
        read_u32(block, offset + 8)?,
        read_u32(block, offset + 12)?,
    ])
}

fn ascii_at(block: &[u8], offset: usize, max: usize) -> Option<String> {
    let slice = block.get(offset..(offset + max).min(block.len()))?;
    let end = slice
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(slice.len());
    let text: String = slice[..end]
        .iter()
        .filter(|byte| byte.is_ascii_graphic() || **byte == b' ')
        .map(|byte| char::from(*byte))
        .collect();
    let text = text.trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}

fn first_number(text: &str) -> Option<f32> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(text.len());
    text[..end].parse::<f32>().ok()
}

fn captured_from_name(name: &str) -> Option<String> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    Some(format!(
        "{}-{}-{}",
        &digits[0..4],
        &digits[4..6],
        &digits[6..8]
    ))
}

fn decode_thumbnail(jpeg: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    if usize::from(info.width) != 240 || usize::from(info.height) != 320 {
        return None;
    }
    let mut rgba = Vec::with_capacity(240 * 320 * 4);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            for pixel in pixels.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            for value in pixels {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        _ => return None,
    }
    if rgba.len() != 240 * 320 * 4 {
        return None;
    }
    Some(rgba)
}

fn write_png(path: &Path, rgba: &[u8]) -> UsbResult<()> {
    let file =
        fs::File::create(path).map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    let mut encoder = png::Encoder::new(file, 240, 320);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    writer
        .write_image_data(rgba)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn synthetic_container() -> Vec<u8> {
        let mut jpeg1 = vec![0u8; 64];
        jpeg1[0] = 0xFF;
        jpeg1[1] = 0xD8;
        jpeg1[62] = 0xFF;
        jpeg1[63] = 0xD9;

        let mut jpeg2 = vec![0u8; 80];
        jpeg2[0] = 0xFF;
        jpeg2[1] = 0xD8;
        jpeg2[78] = 0xFF;
        jpeg2[79] = 0xD9;

        let mut metadata = vec![0u8; USB_METADATA_BYTES];
        metadata[3_970..3_973].copy_from_slice(b"1.0");
        metadata[3_975..3_979].copy_from_slice(b"T2.0");
        metadata[3_981..3_986].copy_from_slice(b"1.1.9");
        metadata[3_990..3_998].copy_from_slice(b"UTi260B+");
        metadata[4_000..4_004].copy_from_slice(b"0.95");
        metadata[4_012..4_018].copy_from_slice(&[0x01, 0xA1, 0x6F, 0xB3, 0x35, 0x4F]);
        metadata[4_018..4_023].copy_from_slice(b"0.25m");
        metadata[4_028..4_033].copy_from_slice(b"55%RH");
        metadata[4_038..4_044].copy_from_slice(b"25_0_0");
        metadata[4_048..4_055].copy_from_slice(b"256x192");
        metadata[4_058..4_059].copy_from_slice(b"0");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&jpeg1);
        bytes.extend_from_slice(&(jpeg2.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&jpeg2);
        bytes.extend_from_slice(&(jpeg2.len() as u32).to_be_bytes());
        for index in 0..TEMP_PLANE_SAMPLES {
            let value = 20.0 + (index % 100) as f32 / 10.0;
            bytes.extend_from_slice(&value.to_be_bytes());
            bytes.extend_from_slice(&(index as u32 % 254).to_be_bytes());
        }
        bytes.extend_from_slice(&metadata);
        bytes
    }

    pub(crate) fn synthetic_thermal_only_container() -> Vec<u8> {
        let dual = synthetic_container();
        let jpeg1_end = find_eoi(&dual, 2).unwrap();
        let len2 = read_u32(&dual, jpeg1_end).unwrap() as usize;
        let records_start = jpeg1_end + 4 + len2 + 4;
        let mut bytes = dual[..jpeg1_end].to_vec();
        bytes.extend_from_slice(&(-1i32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&dual[records_start..]);
        bytes
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "uti260b-usb-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn parses_container_and_metadata() {
        let bytes = synthetic_container();
        let image = parse_usb_bytes(Path::new("202610240001E.jpg"), &bytes).expect("parse");
        assert_eq!(image.temperature_c.len(), TEMP_PLANE_SAMPLES);
        assert_eq!(image.aux.len(), TEMP_PLANE_SAMPLES);
        assert!((image.temperature_c[0] - 20.0).abs() < 1e-6);
        assert_eq!(image.metadata.firmware, "1.1.9");
        assert_eq!(image.metadata.model, "UTi260B+");
        assert_eq!(image.metadata.emissivity, Some(0.95));
        assert_eq!(image.metadata.distance_m, Some(0.25));
        assert_eq!(image.metadata.humidity_pct, Some(55));
        assert_eq!(image.metadata.ambient, Some(25.0));
        assert_eq!(image.metadata.unit, Some(0));
        assert_eq!(image.metadata.resolution.as_deref(), Some("256x192"));
        assert_eq!(image.captured.as_deref(), Some("2026-10-24"));
        assert_eq!(image.sha256.len(), 64);
        assert!(image.thumbnail_rgba.is_none(), "stub JPEG is not decodable");
    }

    #[test]
    fn parses_thermal_only_container_without_visible_jpeg() {
        let bytes = synthetic_thermal_only_container();
        let image = parse_usb_bytes(Path::new("shutter.jpg"), &bytes).expect("parse");
        assert!(image.jpeg2.is_empty());
        assert_eq!(image.temperature_c.len(), TEMP_PLANE_SAMPLES);
        assert!((image.temperature_c[0] - 20.0).abs() < 1e-6);
    }

    #[test]
    fn accepts_firmware_at_both_observed_offsets() {
        let mut bytes = synthetic_container();
        let jpeg1_end = find_eoi(&bytes, 2).unwrap();
        let len2 = read_u32(&bytes, jpeg1_end).unwrap() as usize;
        let metadata_start = jpeg1_end + 4 + len2 + 4 + USB_RECORDS_BYTES;
        // Move the firmware one byte earlier, as on the real USB sample.
        bytes[metadata_start + 3_981..metadata_start + 3_986].copy_from_slice(b"\0\0\0\0\0");
        bytes[metadata_start + 3_980..metadata_start + 3_985].copy_from_slice(b"1.1.9");
        let image = parse_usb_bytes(Path::new("202610240001E.jpg"), &bytes).expect("parse");
        assert_eq!(image.metadata.firmware, "1.1.9");
    }

    #[test]
    fn rejects_length_word_mismatch() {
        let mut bytes = synthetic_container();
        let jpeg1_end = find_eoi(&bytes, 2).unwrap();
        let len2 = read_u32(&bytes, jpeg1_end).unwrap();
        let second_len_offset = jpeg1_end + 4 + len2 as usize;
        bytes[second_len_offset..second_len_offset + 4].copy_from_slice(&(len2 + 1).to_be_bytes());
        let err = parse_usb_bytes(Path::new("bad.jpg"), &bytes).unwrap_err();
        assert!(err.contains("length words disagree"), "{err}");
    }

    #[test]
    fn rejects_size_equation_mismatch() {
        let mut bytes = synthetic_container();
        bytes.push(0);
        let err = parse_usb_bytes(Path::new("bad.jpg"), &bytes).unwrap_err();
        assert!(err.contains("size equation failed"), "{err}");
    }

    #[test]
    fn rejects_out_of_band_temperature() {
        let mut bytes = synthetic_container();
        let jpeg1_end = find_eoi(&bytes, 2).unwrap();
        let len2 = read_u32(&bytes, jpeg1_end).unwrap() as usize;
        let records = jpeg1_end + 4 + len2 + 4;
        bytes[records..records + 4].copy_from_slice(&1.0e30f32.to_be_bytes());
        let err = parse_usb_bytes(Path::new("bad.jpg"), &bytes).unwrap_err();
        assert!(err.contains("physical band"), "{err}");
    }

    #[test]
    fn discovers_dcim_volume() {
        let dir = temp_dir("discover");
        let dcim = dir.join("DCIM");
        fs::create_dir_all(&dcim).unwrap();
        fs::write(dir.join("AUTORUN.INF"), "[autorun]\nlabel=UNIT UTi260B+\n").unwrap();
        fs::write(dcim.join("202610240001E.jpg"), synthetic_container()).unwrap();

        assert!(has_camera_autorun(&dir, USB_AUTORUN_MARKER));
        let volumes = discover_volumes(Some(&dir));
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].files.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn autorun_marker_is_configurable() {
        let dir = temp_dir("discover-marker");
        let dcim = dir.join("DCIM");
        fs::create_dir_all(&dcim).unwrap();
        fs::write(dir.join("AUTORUN.INF"), "[autorun]\nlabel=UNIT UTi720B\n").unwrap();
        fs::write(dcim.join("202610240001E.jpg"), synthetic_container()).unwrap();

        assert!(!has_camera_autorun(&dir, USB_AUTORUN_MARKER));
        assert!(has_camera_autorun(&dir, "UNIT UTi720B"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_dedupes_and_caches() {
        let source = temp_dir("import-src");
        let cache = temp_dir("import-cache");
        let file = source.join("202610240001E.jpg");
        fs::write(&file, synthetic_container()).unwrap();

        let mut index = UsbIndex::default();
        let first = import_file(&file, &cache, &mut index).expect("first import");
        assert!(!first.duplicate);
        assert!(cache.join(&first.entry.cache_file).exists());

        let second = import_file(&file, &cache, &mut index).expect("second import");
        assert!(second.duplicate);
        assert_eq!(index.entries.len(), 1);

        let loaded = load_index(&cache);
        assert_eq!(loaded.entries.len(), 1);

        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&cache);
    }
}
