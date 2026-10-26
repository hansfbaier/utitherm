# UTi (UTi_cmdsys) family network protocol — clean-room spec, tiers 1 and 2

Covers the UNI-T thermal imagers that speak the `UTi_cmdsys` command dialect over a single
custom TCP service with a shared message framing. Written to be implementable without reading
vendor code or linking vendor libraries.

Companion documents:

- `UTi260B-plus-protocol-spec.md` — the reference model, hardware-verified end to end
  (framing, key temperatures, `PhotoCap` geometry, palettes, units, USB container).
- `DEVICE-PROFILE.md` — platform, access, calibration layout for the reference unit.
- `DEVICE-SUPPORT-MATRIX.md` (working directory) — how the model list below was derived, with
  the per-model diff evidence.

Evidence classes, used strictly below:

- **[V]** verified on hardware (reference unit, firmware 1.1.9) or on captured bytes.
- **[I]** read from the vendor client's behaviour or constants; structurally certain, not yet
  exercised on that model's hardware.
- **[U]** unknown, with the experiment that would close it.

Nothing here is copied code. Where the vendor client hardcodes a value, this document states
the value and why it must exist, so an independent implementation can derive or confirm it.

---

## 1. Scope

### 1.1 Models in scope

Tier 1 — protocol identical to the reference model, geometry differs only:

| model | DEV_TYPE | raw w x h | display w x h | live pixels |
|---|---|---|---|---|
| UTi260B+ | 112 | 192 x 256 | 240 x 320 | I420 **[V]** |
| UTi720B | 112001 | 192 x 256 | 240 x 320 | I420 **[I]** |
| UTi165B+ | 111 | 120 x 160 | 240 x 320 (180 x 240 variant) | RGB888 **[I]** |
| UTi716B | 111001 | 120 x 160 | 240 x 320 | RGB888 **[I]** |
| UTi320V+ | 106 | 320 x 240 | 640 x 480 | RGB888 **[I]** |
| UTi260V | 107 | 256 x 192 | 640 x 480 | RGB888 **[I]** |

Tier 2 — same transport, framing and command vocabulary; model-specific value surfacing:

| model | DEV_TYPE | raw w x h | display w x h | notes |
|---|---|---|---|---|
| UTi320E | 101 | 320 x 240 | 640 x 480 | `init` parsed for fewer fields |
| UTi260E | 103 | 256 x 192 (older units 260 x 195) | 640 x 480 | geometry is firmware-dependent |
| UTi384H | 102 | 384 x 288 | 640 x 480 | live port 9527 or 9528 — probe both |
| UTi384H JX003 | 110 | 384 x 288 | 640 x 480 | same as UTi384H |
| UTi740H | 102001 | 384 x 288 | 640 x 480 | same as UTi384H |
| UTi260T | 109 | 256 x 192 | 640 x 480 | `PhotoCap` may carry a sixth section |
| UTi720T | 109001 | 256 x 192 | 640 x 480 | same as UTi260T |
| UTi720E / UTi730E | 103001 / 101001 | as E family | 640 x 480 | reuse E-family behaviour |
| UTi720V / UTi730V | 107001 / 106001 | as V family | 640 x 480 | reuse V-family behaviour |

Out of scope: UTi640Q (RTSP 8086), UTi384G / UTi256G (Guide SDK HTTP + RTSP), and the
non-`UTi_cmdsys` families described in the reference spec §1 and §10.

### 1.2 What is shared and what is not

Shared, and therefore specified once below: transport, framing, message ids, the `init`
payload layout, the `vFrame` skeleton, the `PhotoCap` section sequence, the command vocabulary,
and the session rules.

Model-specific: pixel geometry, live pixel encoding, which `init` fields the device fills
meaningfully, whether the live frame carries a temperature plane, and whether `PhotoCap`
carries the optional trailing sections.

---

## 2. Transport and framing

One TCP service per device. Port 9527 for every model in scope, except UTi384H-class which is
9527 in the current client and 9528 in older client generations — probe both, then remember.
Gallery / file transfer is a second port 9528, separate dialect (§8). **[I]**

Framing, both directions, all messages:

```text
token          ASCII, braces included; length is per message type (6, 9 or 11 bytes)
length         u32 BE — total message length, i.e. token + this field + payload + CRLF
payload        message-specific
CRLF           0d 0a, counted inside `length`
```

Rules:

- The length field is the only big-endian scalar in the envelope; payload scalars are
  little-endian except where a field is explicitly documented as f32 BE or u32 BE.
- Validate `length` against bytes actually received before parsing. A short read is a
  framing violation, not a partial message.
- There is no message id, sequence number or checksum. The token identifies the reply type, so
  a client must keep exactly one request outstanding per message type.
- Requests are plain ASCII for control commands and `UTi_cmdsys:<name>` for the three
  data-bearing exchanges (see §3).
- Tokens: `{init}` 6 bytes, `{UTi_img}` 9 bytes, `{UTi_Photo}` 11 bytes. **[V]** on the
  reference model; the same token lengths appear in every model's parser constants. **[I]**

---

## 3. Requests

| request | bytes | reply token |
|---|---|---|
| `UTi_cmdsys:init` | 15 | `{init}` |
| `UTi_cmdsys:vFrame` | 17 | `{UTi_img}` |
| `UTi_cmdsys:PhotoCap` | 19 | `{UTi_Photo}` |
| `UTi_cmdsys:<command>` (+ arguments) | variable | none (§9) |

The device matches requests by substring `UTi_cmd`, so the prefix is mandatory. **[V]**

---

## 4. `init` — universal 148-byte payload

Reply is exactly 160 bytes: 6-byte token + 4-byte length + 148-byte payload + CRLF.

This layout is **identical on all nine models in scope**: the vendor clients declare the same
eleven fixed-size sections in the same order, and the section sizes sum to 148 on every model.
Only the interpretation differs: some models surface fewer fields from the same bytes. **[I]**

| section | size | running offset | content |
|---|---|---|---|
| MarkMenuInfo | 4 | 0 | menu/OSD enable flags, u32 LE (bit0 centre, bit1 max, bit2 min, bit3 data) **[I]** |
| ImageModelInfo | 28 | 4 | imageSelect u32; pip rect 4 x i32 LE (x, y, w, h); fusion f32 LE **[V]** |
| ColorScaleInfo | 24 | 32 | colour-scale limits: two f32 LE at +8 and +12, rest [U] |
| ParamInfo | 20 | 56 | emissivity f32, ambient f32, reflected f32, humidity u32, distance u32 **[V]** |
| TempAlertInfo | 12 | 76 | alarm enable u32, high delta f32 (K), alarm value f32 (°C) **[V]** |
| UnitInfo | 4 | 88 | unit index u32: 0 = °C, 6 = K, 4 unresolved **[V]** |
| CameraInfo | 8 | 92 | two u32, semantics [U] |
| OtherInfo | 8 | 100 | two u32, semantics [U] |
| LaserInfo | 4 | 108 | two i16 LE — laser dot pixel coordinates **[U]** |
| FuseXY | 12 | 112 | three u32, semantics [U] |
| VersionInfo | 24 | 124 | three NUL-padded ASCII strings, e.g. `1.0`, `T2.0`, `1.1.9` **[V]** |

Field-by-field observed values, byte dumps and the cross-check against the USB file footer are
in the reference spec §4.

### 4.1 Per-model surfacing

The bytes are the same; these models read fewer fields **[I]**:

- UTi260E, UTi320E, UTi384H: `ParamInfo` is read as emissivity only (4 of 20 bytes), `UnitInfo`
  is not read.
- UTi640Q (out of scope): neither `ParamInfo` nor `UnitInfo` is read.
- Tier 1 models plus UTi260T read the full `ParamInfo` and `UnitInfo`.

A client should parse the whole 148-byte payload regardless and simply not rely on fields a
given model may leave at zero. Fields that are consumed by all models: version strings,
emissivity, alarm value, unit, `imageSelect`, pip rect, and `ColorScaleInfo` limits.

---

## 5. `vFrame` — live frame

```text
{UTi_img}     9
length        u32 BE, total
pixels        w * h * 3/2  (I420)   or   w * h * 3  (RGB888)
key temps     3 x f32 BE — max, min, centre, in that order     [V]
[plane]       optional temperature plane, see §5.3
CRLF          2
```

### 5.1 Pixel encodings

Two variants exist in the family, and only one appears per model:

| encoding | size | models |
|---|---|---|
| I420 (YUV 4:2:0, planar Y then U then V) | `w*h*3/2` | UTi260B+, UTi720B **[V]** |
| RGB888 (interleaved R, G, B) | `w*h*3` | tier 1 remainder and all tier 2 models **[I]** |

Both are portrait-oriented: `w` is the narrow side (240 for the reference model) and `h` the
long side (320). Conversion for I420 must use the full-range BT.601 matrix, `U` plane before
`V`; the reference spec §5.1 documents the verified colour check.

### 5.2 Length arithmetic — how to tell what arrived

Four numbers are needed before the first frame can be parsed: `display_w`, `display_h`,
`raw_w`, `raw_h`. Take them from the model table (§1.1) so parsing works from the first byte,
then replace them with measured values once a `PhotoCap` reply has been decoded (§7). Do not
consult the model table again after that.

With `body` = message minus token (9), length field (4) and CRLF (2):

```text
body_len = total - 15

for pix_len, fmt in [ (display_w*display_h*3/2, "I420"),
                      (display_w*display_h*3,   "RGB888") ]:
    tail = body_len - 12 - pix_len          # 12 = three key-temperature floats
    if tail == 0                        -> accept(fmt, no plane)
    if tail == raw_w*raw_h*2            -> accept(fmt, u16 plane)
    if tail == raw_w*raw_h*4            -> accept(fmt, f32 plane)
reject()                                    # nothing fits: framing violation, drop connection
```

`w*h*3/2` and `w*h*3` never coincide for the sizes in scope, so this resolves the encoding
without a flag. Definitions of the accepted branches:

| item | definition |
|---|---|
| I420 | planar Y, then U, then V; `3/2` bytes per displayed pixel; portrait, `U` before `V` |
| RGB888 | interleaved R, G, B; row stride `display_w*3`, no padding, top-left origin |
| u16 plane | little-endian, row stride `raw_w`, `raw_h` rows, same orientation as the f32 plane; a device-scaled sensor value, not degrees Celsius, and not convertible by a fixed formula (§6.1 of the reference spec explains why) |
| f32 plane | little-endian degrees Celsius, row stride `raw_w`, `raw_h` rows, ready for the §7 mapping |

### 5.3 Optional temperature plane

After the key temperatures the frame may carry a per-pixel plane of the raw sensor grid:

| tail size | plane content |
|---|---|
| `0` | none — key temperatures only |
| `raw_w * raw_h * 2` | u16, device-scaled, same meaning as the `PhotoCap` raw plane |
| `raw_w * raw_h * 4` | f32 LE, degrees Celsius, same meaning as the `PhotoCap` temperature plane |

Selection is by tail size; the 4-byte variant supersedes the 2-byte one when both could fit.
On the reference model the plane is not a rare fallback — it arrives **once every 11 frames**.
Measured over 70 consecutive requests: extended frames at indices 3, 14, 25, 36, 47, 58, 69,
with 311 835 bytes total (payload 311 820) and a 192x256 f32 plane, about 0.42 s apart **[V]**.

Which models emit it is **[U]** (E20): the mechanism exists in the B+-family client, and the
reference unit exercises it. Treat the plane as a bonus rather than a contract — parse by
tail size, keep the newest plane, and fall back to `PhotoCap` (§6) or USB files when a frame
arrives without one.

### 5.4 Key temperatures

Three f32 BE values: maximum, minimum, centre — in that order for the reference model **[V]**.
They are computed by the device over its own measurement region and do not necessarily equal
max/min/centre of a `PhotoCap` plane captured at a different moment. Ordering across other
models is **[I]**; the semantics are stable, the exact ordering should be confirmed with a warm
target if it matters (E4).

### 5.5 Pacing

One frame in flight. The device triggers a producer and drops a request that arrives while a
frame is being built, so over-polling loses frames silently rather than corrupting them.
Measured 37–47 ms per frame (~21 fps) on the reference model; keep one outstanding request and
report achieved fps, not requested fps. Send `UTi_cmdsys:HeartBack` when idle — no reply. **[V]**

---

## 6. `PhotoCap` — full-resolution capture

```text
{UTi_Photo}   11
length        u32 BE, total
section 0     u32 BE length + data    screenshot (JPEG, display size)
section 1     u32 BE length + data    per-pixel temperature, f32 LE, raw grid
section 2     u32 BE length + data    visible/clean image (JPEG, larger)
section 3     u32 BE length + data    raw sensor plane, u16 LE, raw grid
section 4     u32 BE length + data    TM/thermal-metadata block
[section 5]   u32 BE length + data    meterInfo — only on models that emit it
CRLF          2
```

- Section lengths are authoritative; never assume a fixed size. Validate the running total
  against `length` and stop when the CRLF is reached.
- Sections 0–3 are present on every model in scope. Section 4 (TM) is absent on UTi640Q, which
  is out of scope, so for tiers 1–2 assume five sections and tolerate a sixth.
- Section 5 (`meterInfo`) appears on UTi260T/UTi720T. Detect it by remaining bytes after
  section 4 — a trailing-compatible extension, not a flag. Its content is **[U]**.
- Reference measurements on UTi260B+: total 563 661 and 584 574 bytes across captures; sections
  54 057 / 196 608 / 214 647 / 98 304 / 8 in the first capture. Section 1 is always
  `raw_w * raw_h * 4`. **[V]**

### 6.1 Geometry and orientation

- Section 1 row stride is `raw_w`, not the display width: 192 for the reference model. Reading
  it as 256 tiles the image into bands — the classic failure mode. Confirm by autocorrelation
  (peaks at lag `raw_w`).
- The plane maps to the display with no rotation and no flip, scale `display_w / raw_w`
  (1.25 on the reference model). Verified by luma correlation +0.828 against the clean JPEG.
- Section 1 is authoritative for temperature. Section 3 (u16) is not linear:
  `T = 0.020779*u16 + 29.4219` leaves 0.24 °C residual because non-uniformity correction is
  already applied to the f32 plane. Use u16 for diagnostics only. **[V]**

### 6.2 Cost and concurrency

Seconds, not milliseconds: 3.3 s on an idle link, 10.4 s with a USB gadget bound. The device
sleeps ~50 ms between the command and the capture and rejects a capture while another is
running. Measured on the reference model there are two failure shapes: a request during an
active capture is dropped silently, while a capture engine that has already failed latches a
busy state and replies with an **empty 17-byte `{UTi_Photo}` frame** (token + u32 17 + CRLF, no
payload) to every further request, for at least 30 s, while `vFrame` keeps working normally.
Detect it by length before parsing the token. Recovery needs an application restart. Worse, aggressive concurrency resets the connection and
on repeated offence kills the camera application outright, requiring a power cycle: no second
capture, no second connection and no other request while a capture is in flight (reference spec
§6.2 documents the measured sequences). A refused 9527 while telnet still answers means the
application died, not that the network failed. A network capture does **not** create a file on the camera's storage — verified
by listing the volume before and after. **[V]**

---

## 7. Geometry discovery

Geometry is never transmitted. The vendor clients hardcode it per model. Two independent ways
to obtain it without a model table:

1. **From a `PhotoCap`** — section 1 length / 4 = `raw_w * raw_h`; the JPEG headers of sections
   0 and 2 give the display size. Combined with the aspect ratio known from the model name,
   this pins both numbers exactly.
2. **From the device identity** — the AP SSID (`UTi260B+`, `UTi320V+`, …) or the USB SCSI model
   string (`UTi260BPlus`), then the table in §1.1.

A client should prefer (1) and cache the result, because geometry is firmware-dependent: the
same model name shipped as 260 x 195 in an older client generation and 256 x 192 in the current
one. **[I]**

Sanity checks after discovery: `raw_w < raw_h` for every model in scope (portrait sensor grid
on a portrait display for the B+ family, landscape for the E/V family — so instead of assuming
orientation, check that `display_w/raw_w == display_h/raw_h` within rounding, and that section 1
length divides exactly).

---

## 8. Gallery port 9528

Line-oriented ASCII, commands prefixed with `#`, replies carrying binary length-prefixed blobs.
Only the reference model has been probed; it answers `STATUS:Ready` to text that is not a
gallery command, and nothing else has been confirmed. **[I]**

Shape observed in the client's command strings: `#getProductModel()`, `#getPhotoDir(...)`,
`#getPhotofile(id,filename)` terminated by `\r\n`. The serving side waits for a `#` byte before
doing work, so an idle connection is normal. Treat this port as optional: a client that only
needs live view, capture and import never touches it.

---

## 9. Commands

Universal vocabulary for tiers 1–2 — the same 21 command names appear in every model's client,
and the reference device implements them all. Arguments are separated by `+`. **[V]** on
reference, **[I]** elsewhere.

| command | effect | notes |
|---|---|---|
| `init`, `vFrame`, `PhotoCap` | see §4, §5, §6 | |
| `HeartBack` | keep-alive, no reply | send when idle |
| `MenuHide` | toggle on-screen menu visibility | |
| `Shutter` | background/FFC shutter | only honoured while idle |
| `CalibraCam` | calibration entry point | risky; not needed by a viewer |
| `colorChange` | palette index | 0–7 measured on the reference model |
| `measUnit` | unit field + value | `+0+0` = °C, `+0+1` = K; the init byte is a bit field, bit 1 = Kelvin, bit 2 = independent flag (§9.2) |
| `measParam` | emissivity / distance / humidity / ambient | syntax [U] |
| `alert` | alarm enable, high delta, value | |
| `imgModel` | image mode (IR / fusion / PiP) | |
| `markTool` | measurement tool selection | |
| `scaleChange` | colour-scale mode | |
| `scaleUpdata` | colour-scale limits | binary payload, layout [U] (E12) |
| `pipRectMove` | PiP rectangle | binary: `UTi_cmdsys:pipRectMove` + u32 BE length + four u16 LE + `"/n/a"` |
| `photoModel` | photo mode | |
| `brig` | display brightness | |
| `lang` | UI language | |
| `rtc` | set clock | |
| `tf` | storage/format related | semantics [U] |
| `controlStop`, `mousePress`, `mouseRelease` | in the device's dispatcher only | never sent by the vendor client; semantics [U] |

Every request is written `UTi_cmdsys:<name>`, no terminator. The `sys:<name>` strings that
appear in the device's binaries are the handler names it compares *after* stripping the
`UTi_cmd` prefix — they are not wire text. Sending `UTi_cmdsys:sys:<name>` is malformed, and a
malformed command can kill the daemon. **[V]**

Rules that hold across the family:

- Control commands produce **no reply**. Confirm by reading the next `vFrame` (palette, unit,
  geometry changes) or a fresh `init`.
- Argument lists are parsed positionally with no bounds check on the device: a truncated
  argument list aborts the process and takes both ports down until a power cycle. Send only
  well-formed commands, and never send an argument a client has not measured. **[V]**
- Unknown commands are ignored, not answered. Do not interpret silence as success.
- Unit state can disagree with itself. Observed on the reference model: `measUnit+0+1` switches
  the OSD to Kelvin while `init` keeps reporting the old byte, and a following `measUnit+1+0`
  can leave the reported byte at `2` with the OSD still in Kelvin. A client should display the
  reported byte, render values in that unit, and offer a power cycle as recovery rather than
  sending more unit commands to "correct" it.

### 9.1 Unit control and the unit byte

`measUnit+<field>+<value>`: field 0 addresses the temperature unit, field 1 a secondary flag,
and the value is 0 or 1. The `init` byte at offset 88 is a bit field — bit 1 (`0x2`) means
Kelvin, clear means Celsius; bit 2 (`0x4`) is an independent flag whose meaning is **[U]**.

| command | effect |
|---|---|
| `measUnit+0+0` | Celsius, bit 2 untouched |
| `measUnit+0+1` | Kelvin, bit 2 untouched |
| `measUnit+1+0` / `+1+1` | clear / set bit 2, temperature unit untouched |
| `measUnit+2+<n>` | ignored |

Two traps this resolves:

- **Key temperatures in `vFrame` are always Celsius**, whatever the unit byte says. Measured
  identical values (35.67 / 29.64 / 30.43) in both unit states. Only the `init` scalars and the
  OSD follow the unit, so a client must convert for display itself.
- A client that sends `+1+0` to request Celsius sees no temperature change and concludes the
  unit is stuck. It is not: `+1+x` never touches the temperature bit.

### 9.2 Palette signatures are not fingerprints

The per-index RGB/saturation figures in the reference spec §3.1 are one-scene observations that
include the scene and the burned-in OSD. Use them to tell palettes apart, never as an exact
match test: a confirmation step must compare loosely (a shift in mean hue and saturation) or
read the palette state back through `init` where the model supports it.

---

## 10. Session rules

```text
Disconnected -> connect :9527 -> Initialised (init) -> Streaming (vFrame loop)
Streaming -> framing violation / timeout -> Reconnecting (backoff 250 ms .. 2 s) -> re-init
```

- Re-send `init` after every reconnect; device state may change while disconnected.
- Never pipeline. One `vFrame` outstanding, one `PhotoCap` at a time.
- Any framing violation drops the connection and re-runs `init` — the stream is not
  self-synchronising.
- Treat connection-refused plus a dead gallery port as "device process down" (crash), not as a
  network fault; recovery requires a power cycle.

---

## 11. Capability matrix

| capability | live `vFrame` | `PhotoCap` | USB files |
|---|---|---|---|
| palettised image | yes (device-rendered) | yes (two JPEG sizes) | yes |
| per-pixel temperature, frame rate | yes — one frame in 11 on the reference model (§5.3); per-model **[U]** | yes (f32 plane) | yes |
| per-pixel temperature, on demand | no | yes | yes |
| raw sensor plane (u16) | only with the optional 16-bit plane | yes | no |
| aggregated max/min/centre | yes (3 floats) | yes | yes (full matrix) |
| palette / unit / parameter control | yes | n/a | n/a |

A live viewer needs only §2, §5 and §10. A measuring client needs `PhotoCap` (§6) or the USB
import path from the reference spec §11.

---

## 12. Acceptance criteria

1. `init` reply parses to exactly 148 payload bytes and exposes the version strings, emissivity,
   alarm value, unit and pip rectangle.
2. Live frames decode correctly for both I420 and RGB888 without a per-model switch — decided
   by the length arithmetic in §5.2.
3. An optional temperature plane is detected by tail size and, when present, rendered as a
   temperature image using the same geometry as `PhotoCap`; when absent the client says so
   instead of showing an empty overlay.
4. `PhotoCap` parses section lengths independently, tolerates a missing section 4 and an extra
   section 5, and rejects the reply if the running total disagrees with the envelope length.
5. Geometry is discovered at runtime, not compiled in, and cached per device, seeded from the
   model table until the first `PhotoCap` reply arrives (§5.2).
6. At most one `PhotoCap` in flight, tracked by the client, with no second connection and no
   other request during the capture window — the device does not signal "busy", it drops the
   request and can die (§6.2 of the reference spec).
7. Exactly one request of each type is outstanding; achieved fps is reported honestly.
8. Every control command sent is one the client has verified for that device; truncated
   argument lists are impossible by construction (typed command builders, not string
   concatenation). Unit changes use only `measUnit+0+0` / `+0+1`, and displayed temperatures are
   converted from wire Celsius using bit 1 of the `init` byte.
9. A model that leaves `init` fields at zero does not produce a UI showing 0.00 emissivity or
   unit index 0 by accident — fall back to "unknown" instead.

---

## 13. Open questions

| id | question | experiment |
|---|---|---|
| E4 | key-temperature ordering on non-reference models | warm a corner; compare with a `PhotoCap` plane |
| E20 | do other models emit the live temperature plane, and at what period? | capture ≥50 live frames per model, record which indices are extended |
| D1 | is RGB888 really the encoding for tier 1 models other than the B+ family? | one live capture per model |
| D3 | UTi384H live port: 9527, 9528, or both? | port probe |
| D4 | do the `E`/`V`/`T` SKU variants behave like their base class? | `init` capture per SKU |
| E12 | `scaleUpdata` binary layout | decode from client-side construction, verify on device |
| E21 | content of `PhotoCap` section 5 (`meterInfo`) on UTi260T | capture and compare with the TM block |

Each needs one session with the model in hand and a raw `tee` of the socket so the bytes become
a fixture.

---

## Appendix A — Evidence provenance (non-normative)

| id | claim | how established | artifact |
|---|---|---|---|
| A1 | transport, framing, tokens, request strings | live capture on the reference unit, cross-checked against the device's own request handler | reference spec §4, `NATIVE-FINDINGS.md` |
| A2 | universal 148-byte `init` payload | every model's client declares the same eleven fixed-size sections in the same order; sizes sum to 148 on all nine | `analysis/device/device_matrix/` |
| A3 | I420 for the B+ family | captured frame length equals `9+4+w*h*3/2+12+2`; colour conversion verified visually | reference spec §5.1 |
| A4 | RGB888 for the rest | the clients for those models read `w*h*3` interleaved bytes | `analysis/device/device_matrix/diffs.txt` |
| A5 | optional live temperature plane, 2- and 4-byte variants | the B+-family client sizes the plane by `raw_w*raw_h*2` or `*4` and selects by payload length | `analysis/device/device_matrix/ParserFrame_Live2.java` |
| A6 | `PhotoCap` section sequence and optional trailing sections | section-order comparison across all model clients; reference capture on hardware | reference spec §6, `analysis/capture/session1/` |
| A7 | geometry per model | per-model configuration constants in two client generations | `analysis/device/device_matrix/geometry.txt` |
| A8 | command vocabulary shared across models | extracted `UTi_cmdsys:` strings per model; identical set for all tiers 1–2 | `analysis/device/device_matrix/diffs.txt` |
| A9 | crash on truncated argument lists | reproduced on the reference unit | `FINDINGS-2026-10-24-session1.md` |

Vendor identifiers appear only in this appendix, and only to make the evidence traceable.
