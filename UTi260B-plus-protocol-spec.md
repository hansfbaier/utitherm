# UTi260B+ network protocol — clean-room implementation spec

Target device: UNI-T **UTi260B+** thermal imager, firmware `1.1.9` (reported `1.0` / `T2.0` / `1.1.9`).
Purpose: enough behavioural detail to write an independent Rust + egui client without
reading vendor code or linking vendor libraries.

Evidence classes used below:

- **[V]** verified against the live device or captured bytes (`analysis/capture/`).
- **[I]** inferred from the Android client's observable behaviour (command strings, framing
  sizes, parser constants). Not yet exercised on the device.
- **[U]** unknown; experiment proposed in §12.

Nothing here is copied code. Wire formats are byte-level observations; all parsing,
state machines and UI behaviour must be written fresh.

---

## 1. Device and network model

| item | value | class |
|---|---|---|
| AP SSID | `UTi260B+` | [V] |
| Camera IP | DHCP default gateway of the camera AP (`192.168.27.10` in our run) | [V] |
| Client IP | DHCP lease from camera (`192.168.27.11/24`) | [V] |
| Other models | older families use fixed AP addresses (192.168.10.10 … 192.168.14.10, .42.1, .43.1, .45.1) | [I] |

Design rule: do **not** hardcode the camera IP. After joining the AP, read the default
gateway of the Wi-Fi interface that carries the camera SSID. Fall back to manual override.

### Open TCP ports [V]

| port | purpose | notes |
|---|---|---|
| 9527 | live view + control (this spec) | plaintext, no authentication |
| 9528 | gallery / file transfer | separate protocol, §9 |
| 21 | FTP | banner `220 Operation successful` |
| 23 | telnet login prompt `UNI-T login:` | no default credentials found |
| 53 | DNS | present, unused by client |

Ports 80/443/554/8554/8086/23021 are closed on this model. (Those belong to other
families: 8554 RTSP and 23021 HTTP-API on UTi384G/UTi256G, 8086 RTSP on UTi640Q.)

---

## 2. Transport and framing

One TCP connection to `camera:9527` carries all live-view traffic. Requests are
plain ASCII, sent with **no length prefix and no terminator**. The device answers with a
single binary message per request.

```text
reply := request_token  /* ASCII, fixed length per command */
         length         /* u32, big endian, TOTAL message length incl. header+trailer */
         payload        /* length - len(token) - 4 - 2 bytes */
         end_tag        /* 0x0D 0x0A */
```

Rules:

- `length` counts the whole message: token + 4 + payload + 2. Verify with
  `token_len + 4 + payload_len + 2 == length`, reject otherwise.
- End tag is CRLF and is *inside* the declared length. [V]
- Token is not a constant across commands: `{init}` for init, `{UTi_img}` for frames.
  Treat the token as an echo/route identifier, not a CRC. [V]
- One reply per request, in order. No multiplexing, no unsolicited pushes observed. [V]
- The device tolerated back-to-back requests on one connection at ~21 frames/s. [V]

Sockets: set `SO_RCVTIMEO`/`SO_SNDTIMEO` of about 2 s (the Android client uses 2 s
connect and 2 s read), `TCP_NODELAY` on, `SO_KEEPALIVE` on. [I]

---

## 3. Command set

All live commands are ASCII names with the `UTi_cmdsys:` prefix. Requests observed or
named by the client: [I]

```text
UTi_cmdsys:init          session start, returns device state
UTi_cmdsys:vFrame        request exactly one live frame
UTi_cmdsys:PhotoCap      full-resolution capture, returns multi-section payload
UTi_cmdsys:colorChange   palette selection
UTi_cmdsys:imgModel      image mode (IR / fusion / PiP)
UTi_cmdsys:pipRectMove   PiP rectangle
UTi_cmdsys:measParam     emissivity / distance / humidity set
UTi_cmdsys:measUnit      temperature unit
UTi_cmdsys:scaleChange   manual temperature range
UTi_cmdsys:scaleUpdata   range update
UTi_cmdsys:markTool      cursor tool
UTi_cmdsys:alert         alarm thresholds
UTi_cmdsys:brig          brightness
UTi_cmdsys:tf            time format
UTi_cmdsys:rtc           clock set
UTi_cmdsys:lang          UI language
UTi_cmdsys:photoModel    photo mode
```

Request length equals the literal string length (`UTi_cmdsys:init` and
`UTi_cmdsys:vFrame` are both 15 bytes).

Parameterised commands append `+` separated integers, **verified on the device** [V]:

```text
UTi_cmdsys:colorChange+<n>      palette index, effect visible in the next frame
UTi_cmdsys:measUnit+<a>+<n>     unit group and unit index
UTi_cmdsys:scaleChange+<n>
UTi_cmdsys:imgModel+<n>
UTi_cmdsys:markTool+<n>
```

These produce no reply; confirm the effect by examining the next `vFrame`.

Two commands are binary, not ASCII [I]:

```text
pipRectMove : "UTi_cmdsys:pipRectMove" + u32 BE(38) + u16 LE x,y,w,h + "/n/a"
scaleUpdata : binary, layout not yet decoded
```

**Safety rule:** the device exits the 9527/9528 daemon on a malformed command and does not
restart it (observed 2026-10-24 after `UTi_cmdsys:measUnit+1` with a missing argument; a
power cycle was required). Build every command from a typed, whitelisted set — never
interpolate user input into the argument string. Treat the command channel as fragile:
no retry storms, no exploratory argument fuzzing on a device you cannot reboot remotely.

### 3.1 Palette control — `colorChange+<n>` [V]

Eight indices measured by decoding one frame per index and computing mean RGB scale and
mean saturation (max−min channel, 0..255):

| n | mean RGB | saturation | observed character |
|---|---|---|---|
| 0 | 179, 60, 38 | 153 | red/orange ramp |
| 1 | 180, 62, 37 | 155 | near-identical to 0 |
| 2 | 164, 101, 21 | 163 | yellow/orange ramp |
| 3 | 136, 136, 135 | 1.1 | neutral grey, mid level |
| 4 | 180, 176, 176 | 5.3 | near-white grey |
| 5 | 115, 115, 114 | 1.1 | dark grey |
| 6 | 176, 27, 44 | 155 | strongly red |
| 7 | 85, 132, 77 | 96 | green-dominant |

Index-to-marketing-name mapping is unconfirmed; identify palettes by rendering a frame per
index and comparing against the device's own palette bar. Values outside 0..7 are
**unverified and unsafe to send** (see safety rule).

Client design: hold the index in state, send on change, never send during frame read, and
re-read the following frame to confirm before updating the UI.

### 3.2 Unit control — `measUnit+<a>+<n>` [V]

Two integers: a group selector and a unit index. Confirmed effects, read back from the
`init` payload (unit byte at offset 88, ambient at 60, alarm at 84):

| `+a+n` | unit byte | ambient | alarm | interpretation |
|---|---|---|---|---|
| `+1+0` | 0 | 25.0 | 36.7 | Celsius (default) |
| `+1+1` | 4 | 25.0 | 36.7 | unknown scale; numbers not converted |
| `+0+1` | 6 | 298.1 | 309.9 | **Kelvin** (25 °C = 298.15 K, 36.7 °C = 309.85 K) |

Rule for clients: after sending a unit change, re-`init` and trust the returned unit byte,
not the requested value. Render the triple of key temperatures using the reported unit
only after that confirmation step, because `+1+1` reports a new unit byte while still
returning Celsius-valued numbers.

---

## 4. `init` exchange

Request: 15 bytes `UTi_cmdsys:init`. [V]

Reply, 160 bytes total:

```text
7b 69 6e 69 74 7d             "{init}"           6-byte token
00 00 00 a0                   length = 160       u32 BE
<150 bytes payload>
0d 0a                         CRLF
```

Payload layout (offsets relative to payload start). All multi-byte integers and floats
observed as **little endian**; the aggregate `length` field is the only big-endian value
in this message. [V]

| off | size | field | observed value | decoding | class |
|---|---|---|---|---|---|
| 0 | 4 | mark menu flags | `0f 00 00 00` | bit0 centre, bit1 max, bit2 min, bit3 data (low nibble set = all enabled) | [I] |
| 4 | 4 | imageSelect | `0` | 0 = IR only (other values = fusion/PiP) | [I] |
| 8 | 16 | pip rect | `3c,50,78,a0` → 60,80,120,160 | i32 LE x,y,w,h | [V] |
| 24 | 4 | distance | `00 00 80 3e` = 0.25 | f32 LE, metres | [V] |
| 28 | 4 | fusion level | `00 00 00 3f` = 0.5 | f32 LE 0..1 | [I] |
| 32 | 24 | colour scale block | `00..00 6e 56 4b 42 4d f8 fe 41 00..00` | first 8 bytes zero; two f32 LE at +8 and +12 = 50.83 and 32.00 | [U] semantics |
| 56 | 4 | emissivity | `33 33 73 3f` = 0.95 | f32 LE | [V] |
| 60 | 4 | ambient / reflected temp | `00 00 c8 41` = 25.0 | f32 LE, °C | [V] |
| 64 | 4 | second ambient field | `00 00 c8 41` = 25.0 | f32 LE, °C | [U] |
| 68 | 4 | humidity | `37 00 00 00` = 55 | u32 LE, %RH | [V] |
| 72 | 4 | ambient integer | `19 00 00 00` = 25 | u32 LE, °C | [I] |
| 76 | 4 | alarm enable | `0` | u32 LE | [I] |
| 80 | 4 | alarm high delta | `00 00 a0 40` = 5.0 | f32 LE, K | [I] |
| 84 | 4 | alarm value | `cd cc 12 42` = 36.7 | f32 LE, °C | [V] |
| 88 | 4 | unit | `0` | 0 = °C (1 = °F presumed) | [I] |
| 92 | 8 | camera info | `1, 20` | two u32 LE, semantics [U] | [U] |
| 100 | 8 | other info | `1, 70` | two u32 LE, semantics [U] | [U] |
| 108 | 4 | laser position | `a0 00 e5 00` | two i16 LE: 160, 229 — plausible laser dot pixel | [U] |
| 112 | 12 | fuse XY | zeros | three u32 LE | [U] |
| 124 | 24 | version block | `"1.0T2.0\0\0\0\0" "1.1.9\0…"` | three NUL-padded ASCII strings: `1.0`, `T2.0`, `1.1.9` | [V] |
| 148 | 2 | end tag | `0d 0a` | CRLF | [V] |

Client should store all of it, display the version strings, and use emissivity /
distance / humidity / unit as the authoritative measurement parameters. Ignore fields
marked [U] rather than guessing.

Cross-check: the same values (0.95, 0.25 m, 55 %RH, ambient 25, unit 0, resolution
256x192, firmware 1.1.9) appear in the USB file footer (§11), which is an independent
confirmation of the field meanings. [V]

---

## 5. `vFrame` exchange

Request: 15 bytes `UTi_cmdsys:vFrame`. [V]

Reply, 115 227 bytes total:

```text
7b 55 54 69 5f 69 6d 67 7d     "{UTi_img}"       9-byte token
00 01 c1 db                    length = 115227   u32 BE
<pixel block 115200>
<key temps 12>
0d 0a                          CRLF
```

### 5.1 Pixel block — YUV 4:2:0, 240x320, portrait [V]

```text
Y plane   240*320        = 76800 bytes
U plane   (240/2)*(320/2)= 19200 bytes
V plane                   19200 bytes
total                    115200 bytes
```

- Colour is already palette-mapped by the device (rainbow/iron/etc. baked into YUV).
  A client cannot recolour a live frame; it must ask the device to change palette
  (`colorChange`, §3) or change it on the camera.
- The on-screen display (OSD) is burned into the image: temperature readout, Wi-Fi and
  battery icons, palette bar, cursor crosshair, timestamps.
- Conversion that reproduced the image correctly: BT.601 full-range, i.e.

```text
R = Y + 1.402*(V-128)
G = Y - 0.344136*(U-128) - 0.714136*(V-128)
B = Y + 1.772*(U-128)
```

  Sampling: `u = U[(y/2)*(W/2) + x/2]`, same for V. Clamp to 0..255. [V]
- Sensor is 256x192; the streamed image is 240x320. The device rescales and re-orients;
  do not derive pixel coordinates from sensor geometry.
- For overlay work, the matching measurement plane is 192x256 and maps to the display by
  exactly 1.25 (see §6.1). Never assume the plane is 256x192 — it renders as four tiled
  bands if you do.

### 5.2 Key temperatures — 3 x f32 **big endian** [V]

Sample bytes: `42 0e b1 ae | 41 f8 ab cd | 41 fd eb 9d` → 35.67, 31.08, 31.73 °C.

- Big endian here, unlike the `init` payload. Keep the readers separate.
- Observed triples: (35.67, 31.08, 31.73), (35.35, 21.75, 33.18), (33.25, 31.28, 31.81).
  One triple reported 21.75 °C while the same capture's temperature plane had a minimum of
  31.0 °C, so the three floats are **not** max/min/centre of the transmitted plane.
  Ordering and source region remain **[U]** (§12 E4). Display them as unlabelled readings
  or label the first value as "primary" until E4 resolves it.
- These are the only per-frame temperatures in live view. The raw 16-bit temperature
  plane is **not** part of the live message on this model, so arbitrary-point
  measurement is impossible from live frames; use `PhotoCap` (§6) for that.

### 5.3 Pacing

Measured round trip 37–47 ms per frame when polling continuously, i.e. ~21 fps, with one
outstanding request. Wait for the full reply before sending the next request; never
pipeline. [V]

---

## 6. `PhotoCap` exchange — full-resolution capture [V]

Request: `UTi_cmdsys:PhotoCap` (15 bytes). Reply 563 661 bytes in 3.27 s.

```text
token "{UTi_Photo}"  11 bytes
length               u32 BE, total message length
section0 .. section4 each: u32 BE length + that many bytes
endTag               0d 0a
```

| # | length | content | geometry |
|---|---|---|---|
| 0 | 54 057 | JPEG with OSD | 240x320 |
| 1 | 196 608 | **f32 LE temperature, °C** | 49 152 samples, **192 wide x 256 tall** |
| 2 | 214 647 | JPEG, clean image | 480x640 |
| 3 | 98 304 | u16 LE raw sensor plane | 192x256 |
| 4 | 8 | zeros, purpose unknown | — |

### 6.1 Measurement plane geometry and coordinate mapping [V]

- Row stride of the measurement planes is **192**, not 256. Rendering as 256 wide produces
  four tiled bands, which is the visual tell. Confirm with autocorrelation: peaks at
  lag 192/384/768.
- The temperature plane maps onto the 240x320 display with no rotation and no flip;
  luma correlation +0.828 at rot 0 / flip 0 versus +0.531 for the next-best orientation.
  Scale is exactly 1.25: `display(x, y) -> temp(x / 1.25, y / 1.25)`.
- Sample values in the reference capture: 31.02 … 32.92 °C, mean 31.73, standard
  deviation 0.289. The narrow spread is normal for a room-temperature scene; do not
  normalise the colour scale per frame unless the UI asks for it.
- The u16 plane is **not** a linear temperature input: `T = 0.020779*u16 + 29.4219` leaves
  0.24 °C residual, and one u16 value spans 0.459 °C across the frame (non-uniformity
  correction). Use the f32 plane as the temperature source; keep u16 for diagnostics.
- Sections 0 and 2 are ordinary baseline JPEGs; section 1 is the only per-pixel
  temperature source in the entire protocol.
- Cost: one capture takes ~3.3 s and the device writes a file to its SD card. Treat it as a
  user-initiated action, not part of the frame loop.

---

## 7. Session state machine

```text
Disconnected
  └─ connect(tcp) ─────────────► Connected
Connected
  └─ send init, parse reply ───► Initialised   (store versions/params)
Initialised
  └─ vFrame loop ──────────────► Streaming
Streaming
  ├─ read error / timeout / length mismatch ──► Reconnecting
  └─ user stops ──────────────────────────────► Closed
Reconnecting
  └─ backoff 250 ms, 500 ms, 1 s, 2 s (cap) ──► Disconnected
```

Rules:

- Any framing violation (length mismatch, missing CRLF, unknown token, unexpected
  payload size) drops the connection and re-runs `init`. Do not try to resynchronise by
  scanning for the token — the stream is not self-synchronising. [I]
- Re-send `init` after every reconnect; device state may have changed while away. [I]
- Before connecting, the Android client pings the camera IP and requires a Wi-Fi network.
  A cleaner equivalent: attempt TCP connect with a 2 s timeout and treat failure as
  "camera not reachable". [I]

---

## 8. Rust + egui reference architecture

Suggested crates: `eframe`/`egui` for UI, `byteorder` (or `from_le_bytes`/`from_be_bytes`),
`parking_lot`, `arc-swap` for the latest-frame handoff, `crossbeam-channel` for control
message passing, `tracing` for logs, `clap` for CLI flags, `image` for PNG export.
No vendor library is required — the whole protocol fits in a few hundred lines.

### 8.1 Threads

```text
[ net thread ]   blocking std::net::TcpStream
                   loop: write "UTi_cmdsys:vFrame", read exact header, read exact body,
                         validate, convert YUV->RGBA8, publish Arc<Frame>
[ ui thread ]    eframe update(): take newest Arc<Frame>, upload to egui texture, paint
```

- Read exactly `length` bytes: `4 + payload + 2`. Use `read_exact`; never `read`+hope.
- Do conversion off the UI thread. 240x320 is 76 800 pixels — trivial for SIMD-free
  Rust, but keep it out of `update()` so a stalled GPU cannot back-pressure the socket.
- Publish with `ArcSwap<Frame>` so the UI always sees the newest frame and the net
  thread never blocks on a slow paint. Drop older frames; never queue them.
- Frame struct: `{ rgba: Vec<u8> /* 240*320*4 */, key: [f32;3], seq: u64, t_recv: Instant }`.

### 8.2 egui specifics

- Create one `TextureHandle` sized 240x320, `TextureFormat::Rgba8UnormSrgb`,
  `TextureOptions::LINEAR`; call `set()` on each new frame (or `TextureHandle::set_partial`
  if you keep the same size).
- Paint with `ui.image((tex.id(), size))` or `egui::Image::new(&tex)`. Preserve aspect
  ratio 3:4; default to integer scaling (1x/2x) so the OSD text stays crisp.
- Show the key temperatures in a side panel; they are independent of the OSD readout and
  update per frame.
- Status bar: connection state, RTT, achieved fps, dropped frames, device version strings
  from `init`.
- Because the OSD is baked in, the client must not draw its own temperature box over the
  image unless it can also hide the device OSD (unknown capability, §12 E6).
- Optional: export current frame as PNG (`image::save_buffer`), plus raw `.yuv` for
  regression fixtures.

### 8.3 Configuration

```text
[camera]
ip = "auto"            # "auto" = default gateway of the camera AP interface
live_port = 9527
connect_timeout_ms = 2000
read_timeout_ms = 2000
target_fps = 20
```

### 8.4 Cancellation and shutdown

The net thread blocks in `read_exact`. To stop, either set a socket read timeout and
check a stop flag each iteration, or `shutdown(Both)` the socket from the UI thread.
Prefer the timeout loop — it keeps shutdown deterministic on all platforms.

---

## 9. Gallery port 9528 — file transfer [I]

The Android client uses a second, text-oriented command dialect on this port:

```text
cmdStart = "#"
cmdEnd   = "\r\n"
commands:  "#getProductModel()\r\n"
           "#getPhotoDir(i,i,i,i)\r\n"
           "#getPhotofile(id,filename)\r\n"
```

Replies carry the same structs the client parses into image files (screenshot, Y16,
temperature, TM). Binary layout and the exact ASCII/binary boundary are **[U]** (§12 E5).
A first client can omit gallery support entirely: pull files over USB mass storage (§11)
or FTP instead.

---

## 10. What the client cannot do (capability matrix)

| capability | live (9527 vFrame) | PhotoCap | USB files | Guide-SDK families |
|---|---|---|---|---|
| palettised image | yes (baked) | yes (two JPEG sizes) | yes | yes |
| per-pixel temperature | **no** | **yes (f32, 192x256)** | yes (f32, 256x192) | yes (Y16 stream) |
| raw sensor plane | no | yes (u16, 192x256) | no | yes |
| aggregated temperatures | yes (3 unlabelled floats) | yes | yes (full matrix) | yes |
| change palette | yes, `colorChange+0..7` | n/a | n/a | via HTTP API |
| change unit | yes, `measUnit+<a>+<n>` | n/a | n/a | via HTTP API |
| change emissivity/distance/humidity | via `measParam` (syntax [U]) | n/a | n/a | via HTTP API |

That table is the main design constraint: a live viewer is straightforward; a measuring
client needs the capture path or the USB files.

---

## 11. Alternative ingest: USB mass storage [V]

When plugged into USB the camera enumerates as a USB mass-storage gadget
(Alliance/Allwinner `sunxi_usb_udc`, vendor `35d4:a4a5`, NTFS volume, model string
`UTi260BPlus`). Files: `DCIM/YYYYMMDDnnnnnE.jpg` plus `DCIM/sys.ini`.

Each `.jpg` is a radiometric container:

```text
[0]                JPEG #1  240x320   (thumbnail)
                   JPEG #2  480x640   (main image)
                   u32 BE   = byte length of JPEG #2   (209218 in our sample)
                   256*192 x { f32 BE temperature °C, u32 BE aux }
                   4063-byte metadata block
```

- Record count 49 152 = 256x192; sample temperatures 18.78 … 36.20 °C, mean 26.49. [V]
- `aux` field observed 0…253, meaning **[U]**.
- Metadata block offsets observed in one sample (all offsets relative to block start):

| off | content |
|---|---|
| 476, 478 | single bytes, unknown |
| 486 | ASCII `q8` |
| 1479, 1631…1648 | single bytes / small floats, unknown |
| 1660 | `3f b3 33 33` (f32 = 1.4) then ASCII `P1` |
| 1676…1712 | measurement rectangles as u32 BE: P1 `(110,150,20,20)` and `(110,130,20,20)`, P2 similar |
| 3970 | ASCII `1.0` `T2.0` |
| 3980 | ASCII `1.1.9` |
| 3990 | ASCII `UTi260B+` |
| 4000 | ASCII `0.95` (emissivity) |
| 4012 | 6 raw bytes (serial `01 a1 6f b3 35 4f`) then ASCII `0.25m` |
| 4028 | ASCII `55%RH` |
| 4038 | ASCII `25_0_0` (ambient / flags) |
| 4048 | ASCII `256x192` |
| 4058 | ASCII `0` (unit) |

- `DCIM/sys.ini` contains an INI section `[UTi260B%2B]` with `qrcode1..5` /
  `value1..5` keys — five QR-code slots the device can stamp into reports. [V]
- File count/date range in our capture: 16 files, 10.8 MB, device clock ahead of host. [V]

Ingest design: watch the mounted volume with a filesystem watcher or poll `DCIM/` on
demand; parse the container by reading the two JPEG SOI/EOI pairs first, then the length
word, then the fixed 8-byte record array, then the metadata block. Validate
`u32 length == jpeg2_len` as a cheap integrity check. [V]

---

## 12. Open questions and the experiments that close them

Resolved 2026-10-24, firmware 1.1.9 (raw reply kept in
`analysis/capture/session1/photocap_raw.bin`):

| id | question | result |
|---|---|---|
| E1 | `PhotoCap` token and section lengths | token `{UTi_Photo}` (11 B); sections 54 057 / 196 608 / 214 647 / 98 304 / 8 |
| E2 | Argument separator for live commands | `+`, not `(...)`; `colorChange+0..7` and `measUnit+<a>+<n>` verified live |
| E3 | `PhotoCap` section formats | JPEG 240x320, f32 °C 192x256, JPEG 480x640, u16 raw 192x256, 8 unknown bytes |
| E7 | `init` unit reporting | unit byte confirmed 0 = °C, 6 = Kelvin; 4 unresolved |

Still open:

| id | question | experiment | risk |
|---|---|---|---|
| E4 | Meaning and source of the three key-temperature floats | warm one corner with a hand; log all three plus the full plane, then correlate | none |
| E5 | Gallery 9528 request/reply framing and binary layout | send `#getProductModel()\r\n`, then `#getPhotoDir(0,0,0,0)\r\n`, capture replies | read-only |
| E6 | Can the device OSD be disabled? | look for a `markTool`/`photoModel` argument that removes overlays; compare frames | cosmetic |
| E8 | `laser` and `fuseXY` field meaning | enable laser pointer in device menu, diff `init` | none |
| E9 | Idle timeout and max concurrent connections on 9527 | leave a connection idle for minutes; open two connections | none; single connection at a time is suspected |
| E10 | `aux` word in USB records | correlate against a scene with known gradients; compare with live key temps | none |
| E11 | `measUnit` group values and unit `4` | sweep `+a` 0..2 with `+n` 0..3, using the ambient/alarm fields as detector | **malformed arguments kill the daemon (§3)**; send only well-formed pairs and expect a reboot cycle |
| E12 | `scaleUpdata` binary layout | decode from client-side construction, then verify on device | daemon crash risk, same as E11 |
| E13 | `PhotoCap` section 4 (8 zero bytes) | compare across many captures, vary settings | none |

Each experiment must be run with a raw `tee` of the socket so the resulting bytes become a
fixture. Record the device state before and after; assume a power cycle may be needed.

---

## 13. Acceptance criteria for the Rust client

1. Parses `init` with all documented offsets, rejects wrong `length` or missing CRLF, and
   displays `1.0` / `T2.0` / `1.1.9` from the device.
2. Streams ≥ 15 fps sustained with one outstanding request and no frame queueing;
   achieved fps and RTT shown in the UI.
3. Decoded live frames are pixel-comparable to a golden fixture: byte-identical YUV in,
   RGBA out within ±1 per channel after the documented BT.601 matrix.
4. Key temperatures displayed and updated per frame; ordering marked as unverified in the
   UI until E4 confirms it.
5. Reconnect: unplugging the AP or power-cycling the camera returns to streaming within
   10 s without user action, and `init` is re-run.
6. Malformed streams are rejected, not guessed: truncated body, wrong length, wrong
   token, missing CRLF, payload size ≠ 115200.
7. No credential, model string or IP is hardcoded; IP resolves from the default gateway
   with manual override.
8. Raw `.yuv` + `.bin` fixture replay mode (`--replay`) drives the UI identically for
   regression tests without the camera.
9. Measurement overlay: clicking the display maps to the capture plane by
   `plane_x = display_x / 1.25`, `plane_y = display_y / 1.25` (192x256 plane), with no
   rotation or flip; a self-test asserts the documented luma correlation ≥ 0.8 against a
   stored PhotoCap fixture.
10. Command channel is a typed whitelist. No code path can emit an argument the device has
    not been verified to accept; unit tests assert the exact bytes for every command,
    including the binary `pipRectMove` frame.
11. Palette and unit changes are confirmed by re-reading the following frame and `init`
    before the UI state changes; a failed confirmation surfaces an error instead of a
    silent no-op.

---

## 14. Fixtures produced so far

| path | content |
|---|---|
| `analysis/capture/live_clean.yuv` | one full 115 200-byte YUV420 live frame |
| `analysis/capture/live_240x320_uv.png` | decoded reference image (BT.601, U then V) |
| `analysis/capture/frame_rgb.bin` | earlier raw frame, same layout |
| `analysis/capture/session1/photocap_raw.bin` | full 563 661-byte `PhotoCap` reply, header included |
| `analysis/capture/session1/temp_256x192.f32` | temperature plane, f32 LE, 192x256 row-major |
| `analysis/capture/session1/y16_256x192.u16` | raw plane, u16 LE, 192x256 |
| `analysis/capture/session1/shot_240x320.jpg` | section 0, JPEG with OSD |
| `analysis/capture/session1/main_480x640.jpg` | section 2, clean JPEG |
| `analysis/capture/session1/temp_map_192x256.png` | false-colour render of the temperature plane |
| `analysis/capture/session1/*.log` | palette and unit probe transcripts |
| `analysis/*.json` | REA/Ghidra dossiers for the Guide-SDK families (not needed for this client) |

Still missing: raw `init` reply bytes, a full `vFrame` message with header, and any gallery
reply. Those three complete the fixture set.
