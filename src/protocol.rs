//! Byte-level wire protocol for the UNI-T UTi260B+ live-view port (TCP 9527).
//!
//! Implemented from `UTi260B-plus-protocol-spec.md`; section numbers in comments
//! refer to that document. No vendor code is involved.
//!
//! Framing (§2):
//!
//! ```text
//! reply := token (ASCII, e.g. "{init}") | length (u32 BE, whole message)
//!        | payload | CRLF
//! ```
//!
//! `length` must equal `token_len + 4 + payload_len + 2`. Multi-byte fields inside
//! the `init` payload are little endian; the three key temperatures of a `vFrame`
//! payload are big endian. Keep those readers separate.

use std::fmt;
use std::io::{self, Read};

// ---------------------------------------------------------------------------
// Command names (§3). All live commands share the "UTi_cmdsys:" prefix.
// ---------------------------------------------------------------------------

pub const CMD_INIT: &str = "UTi_cmdsys:init";
pub const CMD_VFRAME: &str = "UTi_cmdsys:vFrame";
pub const CMD_PHOTOCAP: &str = "UTi_cmdsys:PhotoCap";
pub const CMD_COLOR_CHANGE: &str = "UTi_cmdsys:colorChange";
/// Idle keep-alive (§5.3). No reply, no payload; safe fire-and-forget.
pub const CMD_HEARTBACK: &str = "UTi_cmdsys:HeartBack";
/// Manual flat-field correction / shutter (§5.5); only while idle.
pub const CMD_SHUTTER: &str = "UTi_cmdsys:Shutter";

/// Reply token for the init exchange [V].
pub const TOKEN_INIT: &str = "{init}";
/// Reply token for a live frame [V].
pub const TOKEN_VFRAME: &str = "{UTi_img}";
/// Reply token for a full-resolution capture [V].
pub const TOKEN_PHOTOCAP: &str = "{UTi_Photo}";

// ---------------------------------------------------------------------------
// Typed command channel (§3, acceptance §13.10)
// ---------------------------------------------------------------------------
//
// The device's 9527 daemon exits on a malformed command and does not restart
// itself (§3 safety rule). There is deliberately no free-form command path:
// every request is built from [`Command`], parameters are enumerated or range
// checked at construction, and the exact wire bytes are unit-tested.

/// Palette indices verified on firmware 1.1.9 (§3.1). The index-to-marketing-name
/// mapping is unconfirmed; descriptions come from decoded reference frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    P0,
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}

impl Palette {
    pub const ALL: [Self; 8] = [
        Self::P0,
        Self::P1,
        Self::P2,
        Self::P3,
        Self::P4,
        Self::P5,
        Self::P6,
        Self::P7,
    ];

    pub fn index(self) -> u8 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
            Self::P3 => 3,
            Self::P4 => 4,
            Self::P5 => 5,
            Self::P6 => 6,
            Self::P7 => 7,
        }
    }

    /// Whitelist: only 0..=7 were measured as safe (§3.1).
    pub fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Self::P0),
            1 => Some(Self::P1),
            2 => Some(Self::P2),
            3 => Some(Self::P3),
            4 => Some(Self::P4),
            5 => Some(Self::P5),
            6 => Some(Self::P6),
            7 => Some(Self::P7),
            _ => None,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::P0 => "red/orange ramp",
            Self::P1 => "red/orange ramp, nearly identical to 0",
            Self::P2 => "yellow/orange ramp",
            Self::P3 => "neutral grey, mid level",
            Self::P4 => "near-white grey",
            Self::P5 => "dark grey",
            Self::P6 => "strongly red",
            Self::P7 => "green dominant",
        }
    }
}

/// Mean RGB and mean saturation measured per palette index (§3.1). Used only to
/// confirm a requested change against the next decoded frame, not as calibration.
pub const PALETTE_SIGNATURES: [(f32, f32, f32, f32); 8] = [
    (179.0, 60.0, 38.0, 153.0),
    (180.0, 62.0, 37.0, 155.0),
    (164.0, 101.0, 21.0, 163.0),
    (136.0, 136.0, 135.0, 1.1),
    (180.0, 176.0, 176.0, 5.3),
    (115.0, 115.0, 114.0, 1.1),
    (176.0, 27.0, 44.0, 155.0),
    (85.0, 132.0, 77.0, 96.0),
];

/// Unit commands verified on the device (§3.2). The unit byte is a bit field:
/// bit 1 selects Celsius/Kelvin, bit 2 is an independent secondary flag.
/// Malformed arguments can kill the daemon, so only these pairs are exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitCommand {
    /// `measUnit+0+0`: clear bit 1 -> Celsius, bit 2 unchanged.
    Celsius,
    /// `measUnit+0+1`: set bit 1 -> Kelvin, bit 2 unchanged.
    Kelvin,
    /// `measUnit+1+0`: clear bit 2, temperature unchanged.
    Flag2Clear,
    /// `measUnit+1+1`: set bit 2, temperature unchanged.
    Flag2Set,
}

impl UnitCommand {
    pub const ALL: [Self; 4] = [
        Self::Celsius,
        Self::Kelvin,
        Self::Flag2Clear,
        Self::Flag2Set,
    ];

    pub fn wire(self) -> &'static str {
        match self {
            Self::Celsius => "UTi_cmdsys:measUnit+0+0",
            Self::Kelvin => "UTi_cmdsys:measUnit+0+1",
            Self::Flag2Clear => "UTi_cmdsys:measUnit+1+0",
            Self::Flag2Set => "UTi_cmdsys:measUnit+1+1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Celsius => "Celsius (+0+0)",
            Self::Kelvin => "Kelvin (+0+1)",
            Self::Flag2Clear => "flag 2 off (+1+0)",
            Self::Flag2Set => "flag 2 on (+1+1)",
        }
    }

    /// Whether a reported unit byte satisfies this command's field.
    pub fn matches_byte(self, unit: u32) -> bool {
        match self {
            Self::Celsius => unit & 0x2 == 0,
            Self::Kelvin => unit & 0x2 != 0,
            Self::Flag2Clear => unit & 0x4 == 0,
            Self::Flag2Set => unit & 0x4 != 0,
        }
    }
}

/// A request that this client is allowed to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Init,
    VFrame,
    PhotoCap,
    HeartBack,
    Shutter,
    ColorChange(Palette),
    MeasureUnit(UnitCommand),
}

impl Command {
    /// Exact bytes to write. Parameterised commands use the verified `+`-separated
    /// form with no terminator (§3).
    pub fn wire(self) -> String {
        match self {
            Self::Init => CMD_INIT.to_owned(),
            Self::VFrame => CMD_VFRAME.to_owned(),
            Self::PhotoCap => CMD_PHOTOCAP.to_owned(),
            Self::HeartBack => CMD_HEARTBACK.to_owned(),
            Self::Shutter => CMD_SHUTTER.to_owned(),
            Self::ColorChange(palette) => {
                format!("{CMD_COLOR_CHANGE}+{}", palette.index())
            }
            Self::MeasureUnit(unit) => unit.wire().to_owned(),
        }
    }

    /// Parameterised commands produce no reply (§3); reading one would stall the
    /// frame loop until the socket timeout.
    pub fn expects_reply(self) -> bool {
        matches!(self, Self::Init | Self::VFrame | Self::PhotoCap)
    }

    pub fn expected_token(self) -> Option<&'static str> {
        match self {
            Self::Init => Some(TOKEN_INIT),
            Self::VFrame => Some(TOKEN_VFRAME),
            Self::PhotoCap => Some(TOKEN_PHOTOCAP),
            Self::HeartBack | Self::Shutter | Self::ColorChange(_) | Self::MeasureUnit(_) => None,
        }
    }

    /// `PhotoCap` takes ~3.3 s on the device (§6), far beyond the frame timeout.
    pub fn is_slow(self) -> bool {
        matches!(self, Self::PhotoCap)
    }

    pub fn describe(self) -> String {
        match self {
            Self::Init => "init".to_owned(),
            Self::VFrame => "vFrame".to_owned(),
            Self::PhotoCap => "PhotoCap".to_owned(),
            Self::HeartBack => "HeartBack".to_owned(),
            Self::Shutter => "Shutter".to_owned(),
            Self::ColorChange(palette) => {
                format!(
                    "colorChange+{} ({})",
                    palette.index(),
                    palette.description()
                )
            }
            Self::MeasureUnit(unit) => unit.label().to_owned(),
        }
    }
}

/// Binary `pipRectMove` frame: command name + `u32` BE total length (38) +
/// four `u16` LE values + `"/n/a"` (§3, [I]).
///
/// This encoder is unit-tested but intentionally unreachable from the UI: the
/// meaning of the rectangle values has not been verified on hardware, so sending
/// it would violate the typed-whitelist rule.
#[cfg(test)]
pub fn encode_scale_update(flag: i32, maximum: f32, minimum: f32) -> Vec<u8> {
    const COMMAND: &str = "UTi_cmdsys:scaleUpdata";
    const SUFFIX: &str = "/n/a";
    let mut out = Vec::with_capacity(42);
    out.extend_from_slice(COMMAND.as_bytes());
    let total = (out.len() + 4 + 4 + 4 + 4 + SUFFIX.len()) as u32;
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&flag.to_le_bytes());
    out.extend_from_slice(&maximum.to_be_bytes());
    out.extend_from_slice(&minimum.to_be_bytes());
    out.extend_from_slice(SUFFIX.as_bytes());
    debug_assert_eq!(out.len(), 42);
    out
}

#[cfg(test)]
pub fn encode_pip_rect_move([x, y, w, h]: [u16; 4]) -> Vec<u8> {
    const COMMAND: &str = "UTi_cmdsys:pipRectMove";
    const SUFFIX: &str = "/n/a";
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(COMMAND.as_bytes());
    let total = (out.len() + 4 + 8 + SUFFIX.len()) as u32;
    out.extend_from_slice(&total.to_be_bytes());
    for value in [x, y, w, h] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(SUFFIX.as_bytes());
    debug_assert_eq!(out.len(), 38);
    out
}

// ---------------------------------------------------------------------------
// Geometry and payload sizes (§4, §5).
// ---------------------------------------------------------------------------

/// Streamed frame width in pixels (portrait YUV, already rotated by the device).
pub const FRAME_W: usize = 240;
/// Streamed frame height in pixels.
pub const FRAME_H: usize = 320;
pub const YUV_Y_LEN: usize = FRAME_W * FRAME_H; // 76 800
pub const YUV_CHROMA_LEN: usize = (FRAME_W / 2) * (FRAME_H / 2); // 19 200
pub const YUV_LEN: usize = YUV_Y_LEN + 2 * YUV_CHROMA_LEN; // 115 200
pub const KEY_TEMP_LEN: usize = 12;
pub const VFRAME_PAYLOAD_LEN: usize = YUV_LEN + KEY_TEMP_LEN; // 115 212

/// Live temperature plane appended by the device on every 11th `vFrame`
/// (observed on firmware 1.1.9): f32 LE, 192x256, 196 608 bytes.
pub const LIVE_PLANE_W: usize = 192;
pub const LIVE_PLANE_H: usize = 256;
pub const LIVE_PLANE_SAMPLES: usize = LIVE_PLANE_W * LIVE_PLANE_H; // 49 152
pub const LIVE_PLANE_BYTES: usize = LIVE_PLANE_SAMPLES * 4; // 196 608
pub const INIT_PAYLOAD_LEN: usize = 148;
pub const INIT_MESSAGE_LEN: u32 = 160;
pub const RGBA_LEN: usize = FRAME_W * FRAME_H * 4;

pub const MAX_TOKEN_LEN: usize = 32;
/// Hard cap on a declared message length. `PhotoCap` is the largest known reply.
pub const MAX_MESSAGE_LEN: u32 = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ProtoError {
    Io(io::Error),
    /// Reply did not start with `{`.
    BadTokenStart(u8),
    TokenTooLong,
    TokenNotAscii,
    /// Ring-fenced token check: a frame token where an init token was expected
    /// means the stream is out of sync.
    UnexpectedToken {
        expected: String,
        got: String,
    },
    /// Declared length smaller than the fixed overhead or larger than the cap.
    BadLength(u32),
    /// Trailing bytes were not CRLF.
    MissingEndTag([u8; 2]),
    BadPayloadLen {
        what: &'static str,
        got: usize,
        want: usize,
    },
    Field {
        name: &'static str,
        why: String,
    },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::BadTokenStart(b) => {
                write!(f, "reply does not start with '{{' (first byte 0x{b:02x})")
            }
            Self::TokenTooLong => write!(f, "reply token longer than {MAX_TOKEN_LEN} bytes"),
            Self::TokenNotAscii => write!(f, "reply token is not printable ASCII"),
            Self::UnexpectedToken { expected, got } => {
                write!(
                    f,
                    "unexpected reply token: expected {expected:?}, got {got:?}"
                )
            }
            Self::BadLength(n) => write!(f, "declared message length {n} is impossible"),
            Self::MissingEndTag(tag) => {
                write!(f, "missing CRLF end tag (got {tag:02x?})")
            }
            Self::BadPayloadLen { what, got, want } => {
                write!(f, "{what}: payload is {got} bytes, expected {want}")
            }
            Self::Field { name, why } => write!(f, "bad field {name}: {why}"),
        }
    }
}

impl std::error::Error for ProtoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtoError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ProtoError>;

// ---------------------------------------------------------------------------
// Message framing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Message {
    pub token: String,
    /// Declared total length (token + 4 + payload + 2).
    pub length: u32,
    pub payload: Vec<u8>,
}

impl Message {
    /// Short one-line description used by the reply inspector.
    pub fn summary(&self) -> String {
        format!(
            "token {:?}, declared {} B, payload {} B",
            self.token,
            self.length,
            self.payload.len()
        )
    }
}

/// Read one framed reply from `reader`.
///
/// If `expected_token` is given, a mismatch is rejected before the payload is
/// read: the stream is not self-synchronising, so the caller must drop the
/// connection on any error (§7).
pub fn read_message<R: Read>(reader: &mut R, expected_token: Option<&str>) -> Result<Message> {
    let mut token = Vec::with_capacity(16);
    let mut byte = [0u8; 1];

    loop {
        reader.read_exact(&mut byte)?;
        if token.is_empty() && byte[0] != b'{' {
            return Err(ProtoError::BadTokenStart(byte[0]));
        }
        if !byte[0].is_ascii_graphic() {
            return Err(ProtoError::TokenNotAscii);
        }
        token.push(byte[0]);
        if byte[0] == b'}' {
            break;
        }
        if token.len() >= MAX_TOKEN_LEN {
            return Err(ProtoError::TokenTooLong);
        }
    }

    let token = String::from_utf8(token).map_err(|_| ProtoError::TokenNotAscii)?;
    if let Some(expected) = expected_token
        && token != expected
    {
        return Err(ProtoError::UnexpectedToken {
            expected: expected.to_owned(),
            got: token,
        });
    }

    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let length = u32::from_be_bytes(len_bytes);

    let overhead = token.len() + 4 + 2;
    let payload_len = (length as usize).checked_sub(overhead);
    let Some(payload_len) = payload_len else {
        return Err(ProtoError::BadLength(length));
    };
    if length > MAX_MESSAGE_LEN {
        return Err(ProtoError::BadLength(length));
    }

    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;

    let mut tag = [0u8; 2];
    reader.read_exact(&mut tag)?;
    if &tag != b"\r\n" {
        return Err(ProtoError::MissingEndTag(tag));
    }

    Ok(Message {
        token,
        length,
        payload,
    })
}

/// Build a framed message. Used for fixture generation and tests.
pub fn encode_message(token: &str, payload: &[u8]) -> Vec<u8> {
    let total = token.len() + 4 + payload.len() + 2;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(token.as_bytes());
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}

// ---------------------------------------------------------------------------
// `init` payload (§4)
// ---------------------------------------------------------------------------

/// Device state returned by `UTi_cmdsys:init`.
///
/// Field meanings are taken from the spec. Fields flagged `[U]` there are kept
/// as raw/opaque data and shown without a guessed meaning in the UI.
#[derive(Debug, Clone, Default)]
pub struct InitReply {
    /// mark/menu flags: bit0 centre, bit1 max, bit2 min, bit3 data [I].
    pub mark_flags: u32,
    /// 0 = IR only, other values fusion/PiP [I].
    pub image_select: u32,
    /// PiP rectangle x, y, w, h in streamed pixels [V].
    pub pip_rect: [i32; 4],
    pub distance_m: f32,
    pub fusion_level: f32,
    /// 24-byte colour-scale block: palette, mode, auto/manual bounds (§4).
    pub color_scale: [u8; 24],
    pub emissivity: f32,
    pub ambient_c: f32,
    /// Second ambient-looking field, semantics unknown [U].
    pub ambient2_c: f32,
    pub humidity_pct: u32,
    /// Ambient as integer [I].
    pub ambient_int_c: u32,
    pub alarm_enable: u32,
    pub alarm_high_delta_k: f32,
    pub alarm_value_c: f32,
    /// 0 = °C, 1 presumed °F [I].
    pub unit: u32,
    /// Two opaque words [U].
    pub camera_info: [u32; 2],
    /// Two opaque words [U].
    pub other_info: [u32; 2],
    /// Laser dot pixel, two i16 [U].
    pub laser_px: [i16; 2],
    /// Three opaque words [U].
    pub fuse_xy: [u32; 3],
    /// NUL-padded ASCII strings: "1.0", "T2.0", "1.1.9" [V].
    pub versions: [String; 3],
    pub raw: Vec<u8>,
}

/// Decoded colour-scale block from `init` (§4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorScale {
    pub palette: u32,
    pub mode: u32,
    pub auto_upper_c: f32,
    pub auto_lower_c: f32,
    pub manual_upper_c: f32,
    pub manual_lower_c: f32,
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("bounds checked"))
}

fn i32_at(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(b[off..off + 4].try_into().expect("bounds checked"))
}

fn f32_at(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(b[off..off + 4].try_into().expect("bounds checked"))
}

fn i16_at(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes(b[off..off + 2].try_into().expect("bounds checked"))
}

/// Trim a fixed-width, NUL-padded ASCII field.
fn cstr_ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ')
        .map(|&b| b as char)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Decode the 24-byte version block at payload offset 124.
///
/// Observed layout packs "1.0" at +0 and "T2.0" at +4 with the third string at
/// +12 (§4). A stricter fixed 8-byte-per-string layout is accepted as a fallback.
pub fn parse_versions(block: &[u8]) -> [String; 3] {
    debug_assert!(block.len() >= 24);
    let packed = block[0] != 0 && block[4] != 0;
    if packed {
        [
            cstr_ascii(&block[0..4]),
            cstr_ascii(&block[4..8]),
            cstr_ascii(&block[12..24]),
        ]
    } else {
        [
            cstr_ascii(&block[0..8]),
            cstr_ascii(&block[8..16]),
            cstr_ascii(&block[16..24]),
        ]
    }
}

impl InitReply {
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() != INIT_PAYLOAD_LEN {
            return Err(ProtoError::BadPayloadLen {
                what: "init",
                got: payload.len(),
                want: INIT_PAYLOAD_LEN,
            });
        }

        let mut color_scale = [0u8; 24];
        color_scale.copy_from_slice(&payload[32..56]);

        let reply = Self {
            mark_flags: u32_at(payload, 0),
            image_select: u32_at(payload, 4),
            pip_rect: [
                i32_at(payload, 8),
                i32_at(payload, 12),
                i32_at(payload, 16),
                i32_at(payload, 20),
            ],
            distance_m: f32_at(payload, 24),
            fusion_level: f32_at(payload, 28),
            color_scale,
            emissivity: f32_at(payload, 56),
            ambient_c: f32_at(payload, 60),
            ambient2_c: f32_at(payload, 64),
            humidity_pct: u32_at(payload, 68),
            ambient_int_c: u32_at(payload, 72),
            alarm_enable: u32_at(payload, 76),
            alarm_high_delta_k: f32_at(payload, 80),
            alarm_value_c: f32_at(payload, 84),
            unit: u32_at(payload, 88),
            camera_info: [u32_at(payload, 92), u32_at(payload, 96)],
            other_info: [u32_at(payload, 100), u32_at(payload, 104)],
            laser_px: [i16_at(payload, 108), i16_at(payload, 110)],
            fuse_xy: [
                u32_at(payload, 112),
                u32_at(payload, 116),
                u32_at(payload, 120),
            ],
            versions: parse_versions(&payload[124..148]),
            raw: payload.to_vec(),
        };

        // Reject nonsense that would poison the UI. These are structural sanity
        // checks only; per-field semantics remain as documented.
        for (name, value) in [
            ("distance", reply.distance_m),
            ("fusion", reply.fusion_level),
            ("emissivity", reply.emissivity),
            ("ambient", reply.ambient_c),
            ("ambient2", reply.ambient2_c),
            ("alarm delta", reply.alarm_high_delta_k),
            ("alarm value", reply.alarm_value_c),
        ] {
            if !value.is_finite() {
                return Err(ProtoError::Field {
                    name,
                    why: format!("non-finite float {value}"),
                });
            }
        }

        Ok(reply)
    }

    /// Automatic upper/lower bounds from the colour-scale block.
    pub fn color_scale_floats(&self) -> (f32, f32) {
        (f32_at(&self.color_scale, 8), f32_at(&self.color_scale, 12))
    }

    /// Decode the colour-scale block: palette index, scale mode, and the
    /// automatic/manual bounds (§4). All fields are little endian.
    pub fn color_scale_fields(&self) -> ColorScale {
        ColorScale {
            palette: u32_at(&self.color_scale, 0),
            mode: u32_at(&self.color_scale, 4),
            auto_upper_c: f32_at(&self.color_scale, 8),
            auto_lower_c: f32_at(&self.color_scale, 12),
            manual_upper_c: f32_at(&self.color_scale, 16),
            manual_lower_c: f32_at(&self.color_scale, 20),
        }
    }

    /// Unit byte at payload offset 88 is a bit field (§3.2): bit 1 is the
    /// temperature unit (clear Celsius, set Kelvin); bit 2 is an independent
    /// secondary flag. Unknown high bits make the label untrusted.
    pub fn is_kelvin(&self) -> bool {
        self.unit & 0x2 != 0
    }

    pub fn secondary_flag(&self) -> bool {
        self.unit & 0x4 != 0
    }

    pub fn unit_label(&self) -> &'static str {
        if self.unit & !0x6 != 0 {
            return "unknown unit";
        }
        match (self.is_kelvin(), self.secondary_flag()) {
            (false, false) => "°C",
            (true, false) => "K",
            (false, true) => "°C + flag 2 [U]",
            (true, true) => "K + flag 2 [U]",
        }
    }

    /// Symbol for a temperature after converting the wire value. Key
    /// temperatures in `vFrame` are always Celsius (§3.2).
    pub fn unit_symbol(&self) -> Option<&'static str> {
        if self.unit & !0x6 != 0 {
            None
        } else if self.is_kelvin() {
            Some("K")
        } else {
            Some("°C")
        }
    }

    /// Convert a wire Celsius value to the unit selected by bit 1.
    pub fn display_temperature_c(&self, celsius: f32) -> f32 {
        if self.is_kelvin() {
            celsius + 273.15
        } else {
            celsius
        }
    }
}

// ---------------------------------------------------------------------------
// `vFrame` payload (§5)
// ---------------------------------------------------------------------------

/// Borrowed live-frame payload: YUV 4:2:0 pixel block + three key temperatures.
#[derive(Debug, Clone, Copy)]
pub struct VFrame<'a> {
    pub yuv: &'a [u8],
    /// Big-endian `f32` triple. Not max/min/centre of the measurement plane and
    /// ordering unknown (§5.2, E4); display as unlabelled readings.
    pub key_c: [f32; 3],
    /// Optional live temperature plane (f32 LE, 192x256) appended on every 11th
    /// reply by the device. Lays out after the key temperatures.
    pub plane: Option<&'a [u8]>,
}

pub fn parse_vframe(payload: &[u8]) -> Result<VFrame<'_>> {
    let plane_len = payload.len().checked_sub(VFRAME_PAYLOAD_LEN);
    let plane = match plane_len {
        Some(0) => None,
        Some(LIVE_PLANE_BYTES) => Some(&payload[VFRAME_PAYLOAD_LEN..]),
        _ => {
            return Err(ProtoError::BadPayloadLen {
                what: "vFrame (115212 or 311820)",
                got: payload.len(),
                want: VFRAME_PAYLOAD_LEN,
            });
        }
    };
    let (yuv, key) = payload[..VFRAME_PAYLOAD_LEN].split_at(YUV_LEN);
    let key_c = [
        f32::from_be_bytes(key[0..4].try_into().expect("bounds checked")),
        f32::from_be_bytes(key[4..8].try_into().expect("bounds checked")),
        f32::from_be_bytes(key[8..12].try_into().expect("bounds checked")),
    ];
    Ok(VFrame { yuv, key_c, plane })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use std::io::Cursor;

    #[test]
    fn init_message_is_160_bytes_and_roundtrips() {
        let bytes = fixtures::init_message();
        assert_eq!(bytes.len(), INIT_MESSAGE_LEN as usize);
        assert_eq!(bytes[0..6], *b"{init}");
        assert_eq!(u32::from_be_bytes(bytes[6..10].try_into().unwrap()), 160);
        assert_eq!(&bytes[158..160], b"\r\n");

        let mut cur = Cursor::new(&bytes);
        let msg = read_message(&mut cur, Some(TOKEN_INIT)).expect("parse init reply");
        assert_eq!(msg.token, TOKEN_INIT);
        assert_eq!(msg.length, 160);
        assert_eq!(msg.payload.len(), INIT_PAYLOAD_LEN);
        assert_eq!(cur.position(), bytes.len() as u64);
    }

    #[test]
    fn init_fields_match_documented_values() {
        let replied = fixtures::init_reply();
        assert_eq!(replied.mark_flags, 0x0f);
        assert_eq!(replied.image_select, 0);
        assert_eq!(replied.pip_rect, [60, 80, 120, 160]);
        assert_eq!(replied.distance_m, 0.25);
        assert_eq!(replied.fusion_level, 0.5);
        assert_eq!(replied.emissivity, 0.95);
        assert_eq!(replied.ambient_c, 25.0);
        assert_eq!(replied.ambient2_c, 25.0);
        assert_eq!(replied.humidity_pct, 55);
        assert_eq!(replied.ambient_int_c, 25);
        assert_eq!(replied.alarm_enable, 0);
        assert_eq!(replied.alarm_high_delta_k, 5.0);
        assert_eq!(replied.alarm_value_c, 36.7);
        assert_eq!(replied.unit, 0);
        assert_eq!(replied.camera_info, [1, 20]);
        assert_eq!(replied.other_info, [1, 70]);
        assert_eq!(replied.laser_px, [160, 229]);
        assert_eq!(replied.fuse_xy, [0, 0, 0]);
        assert_eq!(replied.versions[0], "1.0");
        assert_eq!(replied.versions[1], "T2.0");
        assert_eq!(replied.versions[2], "1.1.9");
        assert_eq!(replied.unit_label(), "°C");
    }

    #[test]
    fn colour_scale_block_decodes_documented_fields() {
        let reply = fixtures::init_reply();
        let scale = reply.color_scale_fields();
        assert_eq!(scale.palette, 3);
        assert_eq!(scale.mode, 0);
        assert!((scale.auto_upper_c - 33.45).abs() < 1e-4);
        assert!((scale.auto_lower_c - 30.10).abs() < 1e-4);
        assert!((scale.manual_upper_c - 0.94).abs() < 1e-4);
        assert!(scale.manual_lower_c.abs() < 1e-4);
    }

    #[test]
    fn unit_byte_is_a_bit_field() {
        let mut reply = fixtures::init_reply();
        for (byte, label, kelvin, flag) in [
            (0, "°C", false, false),
            (2, "K", true, false),
            (4, "°C + flag 2 [U]", false, true),
            (6, "K + flag 2 [U]", true, true),
        ] {
            reply.unit = byte;
            assert_eq!(reply.unit_label(), label, "byte {byte}");
            assert_eq!(reply.is_kelvin(), kelvin, "byte {byte}");
            assert_eq!(reply.secondary_flag(), flag, "byte {byte}");
        }
        assert_eq!(reply.unit_symbol(), Some("K"));
        assert!((reply.display_temperature_c(25.0) - 298.15).abs() < 1e-4);
        reply.unit = 1;
        assert_eq!(reply.unit_label(), "unknown unit");
        assert_eq!(reply.unit_symbol(), None);
    }

    #[test]
    fn unit_commands_match_their_field() {
        assert!(UnitCommand::Celsius.matches_byte(0));
        assert!(UnitCommand::Celsius.matches_byte(4));
        assert!(!UnitCommand::Celsius.matches_byte(2));
        assert!(UnitCommand::Kelvin.matches_byte(2));
        assert!(UnitCommand::Kelvin.matches_byte(6));
        assert!(UnitCommand::Flag2Clear.matches_byte(0));
        assert!(!UnitCommand::Flag2Clear.matches_byte(4));
        assert!(UnitCommand::Flag2Set.matches_byte(4));
    }

    #[test]
    fn version_block_fixed_layout_fallback() {
        let mut block = [0u8; 24];
        block[0..3].copy_from_slice(b"1.0");
        block[8..12].copy_from_slice(b"T2.0");
        block[16..21].copy_from_slice(b"1.1.9");
        assert_eq!(parse_versions(&block), ["1.0", "T2.0", "1.1.9"]);
    }

    #[test]
    fn vframe_roundtrips_and_key_temps_are_big_endian() {
        let bytes = fixtures::vframe_message();
        assert_eq!(bytes.len(), 115_227);
        let mut cur = Cursor::new(&bytes);
        let msg = read_message(&mut cur, Some(TOKEN_VFRAME)).expect("parse vframe");
        assert_eq!(msg.payload.len(), VFRAME_PAYLOAD_LEN);

        let frame = parse_vframe(&msg.payload).expect("parse vframe payload");
        assert_eq!(frame.yuv.len(), YUV_LEN);
        // Spec prints the sample rounded; the bytes decode to 31.74 for the third
        // value, so compare at two decimals against the real decode.
        assert_eq!(
            frame.key_c.map(|v| (v * 100.0).round() / 100.0),
            [35.67, 31.08, 31.74]
        );
        // The raw bytes really are big endian.
        assert_eq!(&msg.payload[YUV_LEN..], &fixtures::SAMPLE_KEY_BYTES);
    }

    #[test]
    fn parses_extended_vframe_with_live_plane() {
        let bytes = fixtures::vframe_extended_message();
        assert_eq!(bytes.len(), 311_835);
        let mut cur = Cursor::new(&bytes);
        let msg = read_message(&mut cur, Some(TOKEN_VFRAME)).expect("parse extended");
        assert_eq!(msg.payload.len(), VFRAME_PAYLOAD_LEN + LIVE_PLANE_BYTES);

        let frame = parse_vframe(&msg.payload).expect("extended payload");
        let plane = frame.plane.expect("live plane present");
        assert_eq!(plane.len(), LIVE_PLANE_BYTES);
        assert_eq!(frame.key_c, fixtures::SAMPLE_KEY_C);
        assert_eq!(plane.len() / 4, LIVE_PLANE_SAMPLES);
        assert_eq!(f32::from_le_bytes(plane[0..4].try_into().unwrap()), 31.0);
    }

    #[test]
    fn rejects_wrong_token() {
        let bytes = fixtures::vframe_message();
        let mut cur = Cursor::new(&bytes);
        let err = read_message(&mut cur, Some(TOKEN_INIT)).unwrap_err();
        assert!(matches!(err, ProtoError::UnexpectedToken { .. }), "{err}");
    }

    #[test]
    fn rejects_bad_token_start() {
        let mut cur = Cursor::new(b"\x01\x02\x03\x04".to_vec());
        let err = read_message(&mut cur, None).unwrap_err();
        assert!(matches!(err, ProtoError::BadTokenStart(1)), "{err}");
    }

    #[test]
    fn rejects_length_smaller_than_overhead() {
        let mut bytes = b"{init}".to_vec();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        let mut cur = Cursor::new(bytes);
        let err = read_message(&mut cur, None).unwrap_err();
        assert!(matches!(err, ProtoError::BadLength(4)), "{err}");
    }

    #[test]
    fn rejects_missing_crlf() {
        let mut payload = fixtures::init_payload();
        payload.truncate(INIT_PAYLOAD_LEN);
        let mut bytes = encode_message(TOKEN_INIT, &payload);
        let n = bytes.len();
        bytes[n - 2] = b'\n';
        bytes[n - 1] = b'\r';
        let mut cur = Cursor::new(bytes);
        let err = read_message(&mut cur, None).unwrap_err();
        assert!(matches!(err, ProtoError::MissingEndTag(_)), "{err}");
    }

    #[test]
    fn rejects_truncated_body() {
        let bytes = fixtures::vframe_message();
        let truncated = bytes[..bytes.len() - 10].to_vec();
        let mut cur = Cursor::new(truncated);
        let err = read_message(&mut cur, Some(TOKEN_VFRAME)).unwrap_err();
        assert!(matches!(err, ProtoError::Io(_)), "{err}");
    }

    #[test]
    fn rejects_wrong_payload_size() {
        // Self-consistent framing, but not 115 200 pixel bytes.
        let payload = vec![0u8; 100];
        let bytes = encode_message(TOKEN_VFRAME, &payload);
        let mut cur = Cursor::new(bytes);
        let msg = read_message(&mut cur, Some(TOKEN_VFRAME)).expect("framing is valid");
        let err = parse_vframe(&msg.payload).unwrap_err();
        assert!(
            matches!(err, ProtoError::BadPayloadLen { got: 100, .. }),
            "{err}"
        );

        let err = InitReply::parse(&msg.payload).unwrap_err();
        assert!(matches!(err, ProtoError::BadPayloadLen { .. }), "{err}");
    }

    #[test]
    fn rejects_non_finite_init_float() {
        let mut payload = fixtures::init_payload();
        payload[56..60].copy_from_slice(&f32::NAN.to_le_bytes());
        let err = InitReply::parse(&payload).unwrap_err();
        assert!(matches!(err, ProtoError::Field { .. }), "{err}");
    }

    #[test]
    fn encode_message_matches_documented_frame_size() {
        let payload = vec![0u8; VFRAME_PAYLOAD_LEN];
        let bytes = encode_message(TOKEN_VFRAME, &payload);
        assert_eq!(bytes.len(), 115_227);
        assert_eq!(
            u32::from_be_bytes(bytes[9..13].try_into().unwrap()),
            115_227
        );
    }

    #[test]
    fn command_wire_bytes_are_exact() {
        assert_eq!(Command::Init.wire(), "UTi_cmdsys:init");
        assert_eq!(Command::VFrame.wire(), "UTi_cmdsys:vFrame");
        assert_eq!(Command::PhotoCap.wire(), "UTi_cmdsys:PhotoCap");
        assert_eq!(Command::HeartBack.wire(), "UTi_cmdsys:HeartBack");
        assert_eq!(Command::Shutter.wire(), "UTi_cmdsys:Shutter");

        for (index, palette) in Palette::ALL.into_iter().enumerate() {
            assert_eq!(
                Command::ColorChange(palette).wire(),
                format!("UTi_cmdsys:colorChange+{index}")
            );
        }

        assert_eq!(
            Command::MeasureUnit(UnitCommand::Celsius).wire(),
            "UTi_cmdsys:measUnit+0+0"
        );
        assert_eq!(
            Command::MeasureUnit(UnitCommand::Kelvin).wire(),
            "UTi_cmdsys:measUnit+0+1"
        );
        assert_eq!(
            Command::MeasureUnit(UnitCommand::Flag2Clear).wire(),
            "UTi_cmdsys:measUnit+1+0"
        );
        assert_eq!(
            Command::MeasureUnit(UnitCommand::Flag2Set).wire(),
            "UTi_cmdsys:measUnit+1+1"
        );
    }

    #[test]
    fn command_reply_expectations() {
        assert!(Command::Init.expects_reply());
        assert!(Command::VFrame.expects_reply());
        assert!(Command::PhotoCap.expects_reply());
        assert!(!Command::HeartBack.expects_reply());
        assert!(!Command::Shutter.expects_reply());
        assert_eq!(Command::HeartBack.expected_token(), None);
        assert_eq!(Command::Shutter.expected_token(), None);
        assert!(!Command::ColorChange(Palette::P3).expects_reply());
        assert!(!Command::MeasureUnit(UnitCommand::Celsius).expects_reply());
        assert!(Command::PhotoCap.is_slow());
        assert_eq!(Command::Init.expected_token(), Some(TOKEN_INIT));
        assert_eq!(Command::PhotoCap.expected_token(), Some(TOKEN_PHOTOCAP));
    }

    #[test]
    fn palette_whitelist_is_range_checked() {
        assert!(Palette::from_index(0).is_some());
        assert!(Palette::from_index(7).is_some());
        assert!(Palette::from_index(8).is_none());
        assert!(Palette::from_index(255).is_none());
    }

    #[test]
    fn scale_update_frame_is_exact() {
        let bytes = encode_scale_update(1, 33.45, 0.0);
        assert_eq!(bytes.len(), 42);
        assert_eq!(&bytes[..22], b"UTi_cmdsys:scaleUpdata");
        assert_eq!(u32::from_be_bytes(bytes[22..26].try_into().unwrap()), 42);
        assert_eq!(i32::from_le_bytes(bytes[26..30].try_into().unwrap()), 1);
        assert_eq!(f32::from_be_bytes(bytes[30..34].try_into().unwrap()), 33.45);
        assert_eq!(f32::from_be_bytes(bytes[34..38].try_into().unwrap()), 0.0);
        assert_eq!(&bytes[38..], b"/n/a");
    }

    #[test]
    fn pip_rect_binary_frame_is_exact() {
        let bytes = encode_pip_rect_move([60, 80, 120, 160]);
        assert_eq!(bytes.len(), 38);
        assert_eq!(&bytes[..22], b"UTi_cmdsys:pipRectMove");
        assert_eq!(u32::from_be_bytes(bytes[22..26].try_into().unwrap()), 38);

        let mut values = Vec::new();
        for value in [60u16, 80, 120, 160] {
            values.extend_from_slice(&value.to_le_bytes());
        }
        assert_eq!(&bytes[26..34], &values);
        assert_eq!(&bytes[34..], b"/n/a");
    }
}
