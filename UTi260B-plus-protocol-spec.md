# UTi260B+ network protocol — clean-room implementation spec

Target device: UNI-T **UTi260B+** thermal imager, firmware `1.1.9` (reported `1.0` / `T2.0` / `1.1.9`).
Purpose: enough behavioural detail to write an independent Rust + egui client without
reading vendor code or linking vendor libraries.

Evidence classes used below:

- **[V]** verified against the live device or captured bytes (`analysis/capture/`).
- **[I]** inferred from the Android client's observable behaviour (command strings, framing
  sizes, parser constants). Not yet exercised on the device.
- **[U]** unknown; experiment proposed in §12.

Many claims are also corroborated by the device's own software, examined statically. The
mapping from each normative claim to its source is collected in **Appendix A**; that appendix
is provenance for audit only and is not implementation guidance. The normative text below
describes observable behaviour and deliberately contains no vendor identifiers. Platform and
access context live in `DEVICE-PROFILE.md`. The raw 2026-10-24 session log that produced
§3.1, §3.2, §5.2, §6 and §6.1 has been merged into this document.

Corroboration strengthens a claim but does not relax the clean-room
rule: this document describes behaviour, and every parser, state machine and widget must be
written fresh — no vendor code, no decompiled fragments, no linked vendor libraries.

---

## 1. Device and network model

| item | value | class |
|---|---|---|
| AP SSID | `UTi260B+` | [V] |
| Camera IP | DHCP default gateway of the camera AP (`192.168.27.10` in our run) | [V] |
| Client IP | DHCP lease from camera (`192.168.27.11/24`) | [V] |
| Other models | older families use fixed AP addresses (192.168.10.10 … 192.168.14.10, .42.1, .43.1, .45.1) | [I] |
| AP passphrase | vendor default `12345678` (WPA2-PSK, channel 6) | [V] |

Device platform, credentials, partitions, configuration and calibration files are documented
in `DEVICE-PROFILE.md` (root access confirmed on firmware 1.1.9).

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

Request length equals the literal string length; there is no terminator. Measured wire
lengths:

| request | bytes |
|---|---|
| `UTi_cmdsys:init` | 15 |
| `UTi_cmdsys:vFrame` | 17 |
| `UTi_cmdsys:PhotoCap` | 19 |
| `UTi_cmdsys:HeartBack` | 20 |

**Complete command surface**, established from the device's request handling (Appendix A, A2):

| command | effect |
|---|---|
| `vFrame` | releases the frame producer; one frame in flight, extra requests are latched then dropped |
| `init` | hides the on-screen menu, returns the state payload (§4) |
| `HeartBack` | keep-alive flag, no payload, no reply |
| `colorChange` | palette index |
| `imgModel` | image mode (thermal / fusion / PiP) |
| `markTool` | cursor tool |
| `scaleChange` | range mode |
| `scaleUpdata` | manual range values (binary form) |
| `pipRectMove` | PiP rectangle (binary form) |
| `measParam` | emissivity / distance / humidity |
| `measUnit` | unit group and index (two fields) |
| `alert` | four fields, alarm thresholds |
| `photoModel` | two fields, photo mode |
| `lang` | OSD language |
| `rtc` | clock set |
| `tf` | time format |
| `brig` | brightness, two fields |
| `MenuHide` | OSD/menu visibility |
| `Shutter` | background/FFC shutter, only while idle |
| `PhotoCap` | full-resolution capture (§6); 50 ms delay then capture, rejected while a capture is running |
| `CalibraCam` | calibration entry point |
| `controlStop` | present in the device's dispatcher, never sent by the vendor client; semantics **[U]** |
| `mousePress`, `mouseRelease` | present in the dispatcher, never sent by the vendor client; likely OSD touch forwarding, semantics **[U]** |

Namespace: every request is written `UTi_cmdsys:<name>` on the wire, with no terminator. The
dispatcher rejects anything that does not contain `UTi_cmd`; what it then compares is the
remainder **after** that prefix, which is why the device's own binaries contain the strings
`sys:<name>` rather than the full form. A client never sends `sys:` — sending
`UTi_cmdsys:sys:HeartBack` is malformed, and malformed commands can kill the daemon (§3 safety
rule). [V]

Parameterised commands append `+` separated integers, **verified on the device** [V]:

```text
UTi_cmdsys:colorChange+<n>      palette index, effect visible in the next frame
UTi_cmdsys:measUnit+<a>+<n>     unit group and unit index
UTi_cmdsys:scaleChange+<n>
UTi_cmdsys:imgModel+<n>
UTi_cmdsys:markTool+<n>
```

These produce no reply; confirm the effect by examining the next `vFrame`.

Two commands are binary, not ASCII. `pipRectMove` is verified on the wire **[V]**;
`scaleUpdata` is decoded from the vendor client's own construction and not yet exercised **[I]**:

```text
pipRectMove : "UTi_cmdsys:pipRectMove" + u32 BE(38) + u16 LE x, y, w, h + "/n/a"
scaleUpdata : "UTi_cmdsys:scaleUpdata" + u32 BE(42) + i32 LE flag
              + f32 BE maximum + f32 BE minimum + "/n/a"
```

Observation from the device (2026-10-26): `scaleChange+<n>` for n in {0, 2, 3, 4} left the
reported mode at 0 (auto) with no visible effect, and a `scaleUpdata` write was consequently
ignored — the automatic bounds kept tracking the scene. So the command is only useful once the
device is genuinely in a manual mode, and how to reach that state remotely is still **[U]**
(§12 E12); the message layout itself is settled.

`scaleUpdata` detail: `flag` selects which bound is being written — 1 = maximum, 2 = minimum —
and the field that is not being written is sent as four zero bytes, so the message is always the
same length. Length 42 = 22 (command) + 4 (length) + 4 (flag) + 4 (max) + 4 (min) + 4
(`/n/a`). Note the mixed byte orders, which match the family's habit: the envelope and the
floats are big-endian, the flag and the PiP coordinates are little-endian. The trailing
`/n/a` is a fixed literal, not a placeholder to fill in.

**Safety rule:** the device exits the 9527/9528 daemon on a malformed command and does not
restart it (observed 2026-10-24 after `UTi_cmdsys:measUnit+1` with a missing argument; a
power cycle was required). Build every command from a typed, whitelisted set — never
interpolate user input into the argument string. Treat the command channel as fragile:
no retry storms, no exploratory argument fuzzing on a device you cannot reboot remotely.

### 3.1 Palette control — `colorChange+<n>` [V]

Eight indices measured by decoding one frame per index and computing mean RGB scale and
mean saturation (max−min channel, 0..255). These are **one-scene observations, not palette
fingerprints**: the numbers include the scene and the burned-in OSD, so they shift with
content and cannot be used as a universal confirmation oracle. Use them to tell the palettes
apart, not to verify one:

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

### 3.2 Unit control — `measUnit+<field>+<value>` [V]

Two integers: a field selector and a value. The `init` unit byte (offset 88) is a bit field,
not an enumeration:

| bit | mask | meaning |
|---|---|---|
| 1 | `0x2` | temperature unit: 0 = Celsius, 1 = Kelvin |
| 2 | `0x4` | secondary flag, independent of temperature (semantics **[U]**, §12 E24) |

So the observed bytes 0, 2, 4, 6 are the four combinations, and the reported `ambient`/`alarm`
values follow bit 1: 25.0 / 36.7 in Celsius, 298.1 / 309.9 in Kelvin.

| command | effect on the byte |
|---|---|
| `measUnit+0+0` | clear bit 1 → **Celsius**, bit 2 unchanged |
| `measUnit+0+1` | set bit 1 → **Kelvin**, bit 2 unchanged |
| `measUnit+1+0` | clear bit 2, bit 1 unchanged |
| `measUnit+1+1` | set bit 2, bit 1 unchanged |
| `measUnit+2+<n>` | ignored |

Measured transitions, all consistent with that model: `0 -(0,1)-> 2`, `2 -(0,0)-> 0`,
`4 -(0,1)-> 6`, `6 -(0,0)-> 4`, `6 -(1,0)-> 2`, `4 -(1,0)-> 0`, `0 -(1,1)-> 4`, `2 -(1,1)-> 6`,
and `+2+<n>` changes nothing.

Two consequences that matter more than the encoding itself:

- **Key temperatures are always Celsius on the wire.** The three f32 values in `vFrame` stayed
  at 35.67 / 29.64 / 30.43 in every unit state, including while `init` reported Kelvin. Only
  the `init` scalars and the on-screen OSD follow the unit. A client must therefore label and
  convert them itself, from bit 1, and must not assume the floats are already in the device
  unit.
- To change the temperature unit, send only `+0+0` or `+0+1`. The `+1+*` forms change an
  unrelated flag and were what made earlier sessions look like "the unit is stuck": a client
  that sends `+1+0` to request Celsius gets no temperature change at all, which is exactly what
  the observed no-op looked like.

Rule for clients: after a unit change, re-`init` and trust bit 1 of the returned byte, not the
requested value. Do not attempt to "repair" an unexpected byte by sending more unit commands;
the bit-field model above explains every state seen so far, including the ones that survive a
reboot (the device persists the selection in its own settings).

---|---|---|---|---|
| `+1+0` | 0 | 25.0 | 36.7 | Celsius (default) |
| `+1+1` | 4 | 25.0 | 36.7 | unknown scale; numbers not converted |
| `+0+1` | 6 | 298.1 | 309.9 | **Kelvin** (25 °C = 298.15 K, 36.7 °C = 309.85 K) |
| `+1+0` after `+0+1` | 2 | — | — | **state collision**: the OSD stayed in Kelvin while `init` reported byte 2 |

Rule for clients: after sending a unit change, re-`init` and trust the returned unit byte,
not the requested value. Observed on firmware 1.1.9: `measUnit+0+1` switches the OSD to
Kelvin but `init` keeps reporting the old unit for a while, and a following `measUnit+1+0`
can leave the reported byte at `2` while the OSD stays in Kelvin. The unit state can
therefore disagree with itself; a client must show the raw byte and offer a power cycle as
the recovery path instead of trying to "fix" the unit by sending more commands. Render the triple of key temperatures using the reported unit
only after that confirmation step, because `+1+1` reports a new unit byte while still
returning Celsius-valued numbers.

---

## 4. `init` exchange

Request: 15 bytes `UTi_cmdsys:init`. [V]

Reply, 160 bytes total:

```text
7b 69 6e 69 74 7d             "{init}"           6-byte token
00 00 00 a0                   length = 160       u32 BE
<148-byte payload, offsets 0..147>
0d 0a                         CRLF              outside the payload
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
| 32 | 4 | palette index | `03 00 00 00` = 3 | u32 LE, valid 0..6, same numbering as `colorChange` | [V] |
| 36 | 4 | colour-scale mode | `0` | u32 LE: 0 auto, 1 ratio/span, 2 down, 3 up, 4 section, 5 menu | [I] |
| 40 | 4 | automatic upper bound | f32 LE, e.g. 33.45 | tracks the scene while in automatic modes | [V] |
| 44 | 4 | automatic lower bound | f32 LE, e.g. 30.10 | as above | [V] |
| 48 | 4 | manual upper bound | f32 LE, e.g. 0.94 | only meaningful in modes 2..5; auto modes leave it alone | [I] |
| 52 | 4 | manual lower bound | f32 LE, e.g. 0.00 | as above | [I] |
The 24-byte colour-scale block is therefore `[palette][mode][auto upper][auto lower][manual
upper][manual lower]`, all little-endian — including the two integer fields, despite the
vendor helper being named `bytes2int_Big`. Values outside the documented ranges are clamped by
the camera's own client, so treat an unexpected palette or mode as "unknown", not as data. [V]

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
| 148 | 2 | (outside the payload) | `0d 0a` | CRLF that closes the message | [V] |

The envelope adds up as `6 + 4 + 148 + 2 = 160`: the payload table covers offsets 0..147 and
the CRLF follows it, inside the message and inside `length` but not inside the payload. [V]

Client should store all of it, display the version strings, and use emissivity /
distance / humidity / unit as the authoritative measurement parameters. Ignore fields
marked [U] rather than guessing.

Cross-check: the same values (0.95, 0.25 m, 55 %RH, ambient 25, unit 0, resolution
256x192, firmware 1.1.9) appear in the USB file footer (§11), which is an independent
confirmation of the field meanings. [V]

---

## 5. `vFrame` exchange

Request: 17 bytes `UTi_cmdsys:vFrame`. [V]

Reply, two variants. The common one is 115 227 bytes total:

```text
7b 55 54 69 5f 69 6d 67 7d     "{UTi_img}"       9-byte token
00 01 c1 db                    length = 115227   u32 BE
<pixel block 115200>
<key temps 12>
0d 0a                          CRLF
```

Every **11th frame** appends a full temperature plane, giving 311 835 bytes total
(payload 311 820):

```text
{UTi_img} 9 | length u32 BE = 311835 | pixels 115200 | key temps 12 | plane 196608 | CRLF 2
```

The plane is 192x256 f32 LE in degrees Celsius, row stride 192, no rotation, no flip — the
same product and geometry as `PhotoCap` section 1 (§6.1). Measured over 70 consecutive
requests: planes at frames 3, 14, 25, 36, 47, 58, 69 — a fixed period of 11 frames, about
0.42 s apart at this polling rate. Treat the plane as a bonus, not a contract: parse by length
(§5.2) and use whichever plane is newest.

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
- These three floats are the only per-frame temperatures on the common frame. One frame in
eleven instead carries a full f32 plane (§5), which is enough for arbitrary-point
measurement at about 2 Hz; `PhotoCap` (§6) remains the way to get a plane on demand. A u16 tail is permitted by the protocol (§15.5) but has not been observed on this firmware;
the f32 variant is what this unit sends.

### 5.3 Pacing and streaming semantics

Measured round trip 37–47 ms per frame when polling continuously, i.e. ~21 fps, with one
outstanding request. Wait for the full reply before sending the next request; never
pipeline. [V]

The device-side behaviour explains why, and constrains how a client may pace itself
(Appendix A, A3):

- `vFrame` is a trigger, not a synchronous fetch. The device starts producing a frame and the
  reply arrives when that frame is ready.
- Only **one frame is in flight**. A second request that arrives while a frame is being
  produced is latched as "pending" and then dropped — the device does not queue it. Polling
  faster than the producer therefore loses frames silently rather than corrupting the stream.
- `UTi_cmdsys:HeartBack` is a keep-alive: it is accepted with no reply and no payload. A long-lived
  client should send it during idle periods; the interval is not yet measured (§12 E14).

### 5.4 Device-side temperature chain (reference only) [V]

A client never needs to reproduce this — the `PhotoCap` f32 plane already contains the
corrected result — but the chain explains the numbers and bounds the parameter ranges
(Appendix A, A5):

```text
raw sensor -> NUC table lookup -> Kelvin*16 -> (value / 16.0) - 273.15 = degrees C
                                    table length 0x2000 or 0x4000 entries
```

Coefficient stage `calculate_KE_and_BE` accepts:

| parameter | valid range | unit |
|---|---|---|
| emissivity | 1 … 128 | percent (95 = 0.95) |
| transmittance | 1 … 128 | percent |
| ambient / reflected temp | 230 … 430 (high gain), 230 … 873 (low gain) | 0.1 K |

Out-of-range values return `0xfffffbb4` (`-1100`) from the library, which surfaces as a
missing or sentinel temperature rather than a crash. Transmittance is derived by
`calculate_tau` as `clamp((a-b)/(c-d) * 16384, <= 16384)`, a 14-bit fixed-point ratio.

Consequence: offline recomputation needs the **per-unit** NUC table
(`/mnt/UDISK/release/{high,low}_gain_tpd_nuc_t.bin`) plus the 9-point curve in
`/mnt/private/release/Aligning.ini`. A generic formula will drift from device values. Use the
capture plane.

---

### 5.5 Shutter and flat-field correction (FFC)

The sensor is an uncooled microbolometer: every pixel carries its own offset, and that offset
drifts as the sensor's temperature changes. The camera corrects this with a **flat-field
correction**: a mechanical shutter blade covers the field of view, the sensor images the blade as
a uniform target, and the firmware recomputes the non-uniformity table that is subtracted from
subsequent frames. The audible click is that blade moving. [V]

Mechanism, as found in the device binaries — the names are descriptive enough to specify
behaviour without reading the code:

| piece | role |
|---|---|
| shutter open/close | blade travels across the sensor |
| automatic FFC status and parameters | policy: when to fire |
| automatic FFC timer | periodic trigger |
| manual FFC update | explicit request |
| NUC tables (`*_nuc_t.bin`) | the correction data this procedure refreshes |
| shutterless variants | for cores without a blade; not used on this model |

Triggers, in the order a client will meet them: the timeout fires every minute or two; a large
sensor-temperature drift or scene change fires it early — carrying the camera from a cold room
into a warm one rapidly increases the rate; and `UTi_cmdsys:Shutter` requests one on demand
(§3), honoured only while idle. [I]

What it does to the data, and why a measuring client must care:

- During the cycle the image briefly freezes or shows a flat frame. Those frames are not
  measurements.
- Immediately afterwards **all temperatures step by a small offset**. That step is the
  recalibration, not a change in the scene. [I]
- Consequence: average temperatures are only comparable within one FFC epoch. A client that
  tracks a target over minutes must detect the step and start a new baseline, or force an FFC
  before a measurement series so the whole series shares one epoch.

Detection rule a client can implement without extra protocol support: watch the three key
temperatures while the scene is static; a simultaneous step on all three larger than a few
tenths of a degree, with no accompanying change in the image structure, is an FFC boundary.
Discard the frames around it and re-baseline.

## 6. `PhotoCap` exchange — full-resolution capture [V]
Request: `UTi_cmdsys:PhotoCap` (19 bytes). Reply 563 661 bytes in 3.27 s.

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
- The u16 plane is little-endian with row stride 192, and it is **not** a transfer function
  for temperature. A least-squares fit over the reference capture gives
  `T = 0.020779*u16 + 29.4219` with residual sd 0.060 °C and max 0.24 °C, but identical u16
  codes map to temperatures up to 0.33 °C apart across the frame, because the sensor value
  already carries per-pixel non-uniformity correction. The fit is a fixture-specific
  approximation, not a formula to ship: use the f32 plane and keep u16 for diagnostics.
- Sections 0 and 2 are ordinary baseline JPEGs; section 1 is the only per-pixel
  temperature source in the entire protocol.
- Cost: measured 3.3 s on an idle radio and 10.4 s once with a USB gadget bound — assume
  several seconds. No file is written to the SD card by a network capture (verified: `DCIM/`
  was unchanged after the capture), so `PhotoCap` and the USB files are independent sources.
  Treat a capture as a user-initiated action, not part of the frame loop.
- Concurrency: there is no queue and no reliable error path. Two behaviours were measured, and
  a client must handle both:
  1. a request that arrives while a capture is *actively running* is **silently dropped** — no
     reply, no acknowledgement;
  2. once the capture engine has hit an error (for example because the client disconnected
     mid-capture), it latches a busy state and answers every further request with an **empty
     frame**: `{UTi_Photo}` + u32 `17` + CRLF, 17 bytes total, no payload. That state was
     observed to persist for at least 30 s, with the live stream continuing normally
     throughout.
  Detect it by length, not by token: a 17-byte `{UTi_Photo}` message is "capture unavailable",
  never a capture. See §6.2.
- The device sleeps ~50 ms between the command and the capture, so the returned image is a
  moment after the request, not at it.

### 6.2 Capture concurrency and the busy latch [V]

**The empty reply.** When the capture engine refuses, the answer is 17 bytes:

```text
7b 55 54 69 5f 50 68 6f 74 6f 7d   "{UTi_Photo}"
00 00 00 11                        length = 17
0d 0a                              CRLF
```

No section lengths, no payload. A client that reads the token and then tries to parse sections
will mis-frame its whole stream, so **check the length before the token**: a `{UTi_Photo}`
message of 17 bytes means "busy", anything larger is a capture.

**The latch.** The busy state is sticky. It is entered by an incomplete capture — a client that
disconnects, or a capture that fails — and it does not clear within 30 s of observation. The
live stream keeps working while it is set (`vFrame` continues at the normal 115 227 bytes), so
the device looks healthy on the streaming path and unhealthy only on the capture path. Recovery
observed: restarting the camera application (which also means the on-screen UI had been waiting
on the same capture, which is why the display shows a spinner while this happens).

Client rules:

1. One capture in flight, tracked by the client and never inferred from replies.
2. Never abandon a capture: if the reply times out, keep the connection open, drain whatever
   arrives, and do not start another capture until a full-size reply or a device restart.
3. Treat the 17-byte reply as "capture unavailable", disable the capture function, keep
   streaming, and tell the user the camera's capture engine needs a restart.
4. Do not open a second connection or send other requests during a capture; the failure path
   for that combination has killed the application outright (§6.2.1).

#### 6.2.1 Crash hazard [V]

Captures are also dangerous, independent of the busy latch. Measured sequences and outcomes on
firmware 1.1.9:

| sequence | observed result |
|---|---|
| `PhotoCap`, wait 400 ms, `PhotoCap` again | first capture completes, second silently dropped, connection later torn down |
| two `PhotoCap` requests with no delay between them | one reply, then connection reset |
| `PhotoCap`, then open a **second** connection and send `init` | second connection reset |
| shortly after the above | ports 9527 and 9528 refuse connections, the camera application is **gone from the process table**, and the on-screen UI is left showing its boot spinner |

Nothing is supervising the application: it is started once at boot by an init script with `&`
and has no respawn. Recovery is therefore a **power cycle**, not a reconnect. Telnet and FTP
survive because they are separate daemons, which is a useful diagnostic: if 9527 is refused but
telnet answers, the camera application died rather than the network failing.

Client rules that follow:

1. One capture in flight per device, tracked in the client, not inferred from replies.
2. Never open a second connection to 9527 while a capture is running.
3. Do not send `vFrame` or anything else on the same socket during a capture.
4. If a capture does not arrive as a complete `{UTi_Photo}` message within the timeout, mark the
   device as failed and require a reconnect plus a fresh `init`; do not retry the capture
   immediately.
5. Never send two captures within one second of each other, even on different connections.

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
- One deliberate exception: a `PhotoCap` request may be answered by a short error message
  instead of `{UTi_Photo}` when a capture is already running (§6). Read one complete framed
  message by its length, discard it, and report a retryable "device busy" state. Do not treat
  it as a framing violation, and do not reuse the discarded bytes as a capture. The error
  token and payload are **[U]** (§12 E16); a client must therefore not validate the token
  *before* reading, only after.
- Re-send `init` after every reconnect; device state may have changed while away. [I]
- Keep exactly one `vFrame` outstanding. The device drops extras rather than queueing them
  (§5.3), so pipelining loses frames without an error.
- Send `UTi_cmdsys:HeartBack` when idle for more than a few seconds. The `sys:` forms that
  appear in §3 are handler names inside the device, not wire text; every request carries the
  `UTi_cmdsys:` prefix. It has no reply and no
  payload, so a client cannot distinguish "accepted" from "ignored" — treat it as fire and
  forget (§12 E14).
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
[ import thread ] optional, started when a USB volume appears: copy, validate, index files
[ ui thread ]    eframe update(): take newest Arc<Frame>, upload to egui texture, paint
```

- Read exactly `length` bytes: `4 + payload + 2`. Use `read_exact`; never `read`+hope.
- Do conversion off the UI thread. 240x320 is 76 800 pixels — trivial for SIMD-free
  Rust, but keep it out of `update()` so a stalled GPU cannot back-pressure the socket.
- Publish with `ArcSwap<Frame>` so the UI always sees the newest frame and the net
  thread never blocks on a slow paint. Drop older frames; never queue them.
- Frame struct: `{ rgba: Vec<u8> /* 240*320*4 */, key: [f32;3], seq: u64, t_recv: Instant }`.
- The import worker owns no shared state: it sends progress and finished records over an
  `mpsc` channel and the UI applies them. Never let a mount that vanishes block the network
  thread, and never let a slow JPEG decode stall `update()`.

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
Behaviour that is known (Appendix A, A7): the command channel parses `#`-prefixed text
commands, and the serving side waits for a command byte before it starts doing work — so an
open connection legitimately sits idle until the first `#…\r\n` arrives. That is also why the
port answers `STATUS:Ready` to text that is not a gallery command.
A first client can omit gallery support entirely: pull files over USB mass storage (§11)
or FTP instead.

---

## 10. What the client cannot do (capability matrix)

| capability | live (9527 vFrame) | PhotoCap | USB files | Guide-SDK families |
|---|---|---|---|---|
| palettised image | yes (baked) | yes (two JPEG sizes) | yes | yes |
| per-pixel temperature | **yes**, every 11th frame (f32, 192x256) | **yes (f32, 192x256)** | yes (f32, 192x256, interleaved with `aux`) | yes (Y16 stream) |
| raw sensor plane | no | yes (u16, 192x256) | no | yes |
| aggregated temperatures | yes (3 unlabelled floats) | yes | yes (full matrix) | yes |
| change palette | yes, `colorChange+0..7` | n/a | n/a | via HTTP API |
| change unit | yes, `measUnit+0+<0|1>` (§3.2); wire key temps stay Celsius | n/a | n/a | via HTTP API |
| change emissivity/distance/humidity | via `measParam` (syntax [U]) | n/a | n/a | via HTTP API |

That table is the main design constraint: a live viewer is straightforward; a measuring
client needs the capture path or the USB files. Two practical notes: `PhotoCap` is one-shot
per device (a second concurrent request is rejected as busy, §6), and the f32 plane it returns
is the only temperature source that matches the device's own readout — recomputing from the
u16 plane drifts (§5.4).

---

## 11. Alternative ingest: USB [V]

USB is a **file-ingest path only**: no live stream, no control channel, no per-pixel
measurement beyond what is stored in the files. It is nonetheless the second source of full
radiometric data, and the only one that needs no credentials and no network.

### 11.1 Enumeration and identity

In volume mode the camera is a USB mass-storage gadget:

| property | value |
|---|---|
| VID / PID | `0x35d4` / `0xa4a5` — PID is the generic Linux gadget ID, **not** vendor-unique |
| USB revision / speed | 2.00, high speed (480 Mbit/s negotiated) |
| configuration | one, self-powered, `MaxPower` 2 mA |
| interface | one, class 8 / subclass 6 / protocol `0x50` = mass storage, SCSI transparent, bulk-only |
| endpoints | `0x01` OUT, `0x81` IN, bulk, 512-byte packets |
| string descriptors | kernel defaults: "Linux 4.9.191 with sunxi_usb_udc", "Mass Storage Gadget" |
| SCSI inquiry | vendor `Linux`, model `UTi260BPlus` |

The SCSI model string is the only name that identifies the device — match on VID/PID plus
model, never on PID alone.

The exported block device carries **no partition table**: the NTFS filesystem starts at LBA 0
(a "superfloppy"). Filesystem UUID `585A980C530F08BD`, no volume label. Linux mounts it as
`/run/media/<user>/585A980C530F08BD`, Windows assigns a drive letter, macOS mounts it
read-only by default. Volume root holds `AUTORUN.INF` (33 bytes, `[autorun]` +
`label=UNIT UTi260B+`, a Windows-only label), `DCIM/`, and optionally
`System Volume Information/`.

Measured throughput: **41.5 MB/s** sequential (200 MB read), 30.6 MB/s for a single 650 KB
file. Bulk-only USB 2.0 is the ceiling; do not expect better.

### 11.2 The write hazard

The gadget is bound to `/dev/mmcblk0p10` — the same partition the camera mounts read-write at
`/mnt/SDCARD`. Both sides therefore hold a writable view of one NTFS filesystem with no
locking, and `DCIM/` is the camera's own capture target.

Client rule: **treat the volume as read-only.** Mount read-only explicitly where the platform
allows it, never create/rename/delete files on it, never run a filesystem repair against it.
Copy out, then parse from the host copy.

### 11.3 Volume contents

- `DCIM/YYYYMMDDnnnnnE.jpg` — date, 5-digit counter, `E` suffix. Observed 24 files,
  0.62–0.72 MB each.
- `DCIM/sys.ini` — 173 bytes, QR-code slots (`[UTi260B%2B]`, `qrcode1..5` / `value1..5`).
- Files appear under their final names; no temporary or zero-length placeholders observed.
- The camera refuses to shoot while it is in USB mass-storage mode: the shutter reports that the
  unit is in storage mode. Unplug before capturing, then reattach to read the new files. [V]

### 11.4 Radiometric container [V]

```text
JPEG #1             240x320 RGB, thumbnail with OSD
i32 BE len2         byte length of JPEG #2, or -1 when the visible image is absent
JPEG #2             480x640, clean image, len2 bytes        (only when len2 > 0)
u32 BE len2         the same value repeated, or 0 when absent
49152 x { f32 BE temperature in degrees C, u32 BE aux }    interleaved, 8 bytes each
metadata            4063 bytes, fixed-offset fields
```

**The visible image is optional.** Both variants were captured on the same unit:

| variant | second image | file size |
|---|---|---|
| dual | 480x640 JPEG present | ~650–700 KB |
| thermal-only | absent: `len2` = `ff ff ff ff` (-1), second word `0`, then the records | ~440 KB |

A thermal-only file was produced by the camera's own shutter while the visible-light pipeline was
not running, so the parser must not assume two JPEGs. Detect by scanning for JPEG SOI/EOI pairs:
two means dual, one means the `-1`/`0` fields follow the single JPEG directly. Everything after
them is identical in both variants — 49 152 records, then the 4063-byte metadata. [V]

- 49 152 = 192 x 256. Records are **interleaved**, not planar: four temperature bytes then four
  aux bytes, 49 152 times. Reading the block as a flat float array yields half garbage — a
  cheap self-check is that every decoded temperature must fall inside a physical range.
- Integrity check: `file_size == len(jpeg1) + 4 + len2 + 4 + 8*49152 + 4063`, and both length
  words must equal `len2`. Anything else is a partial read — retry, do not parse.
- Orientation: plane is row-major, 256 rows x 192 columns, no rotation, no flip. Mapping to the
  image is the same as `PhotoCap`, scale 1.25: `image(x, y) -> plane(x/1.25, y/1.25)` — no
  axis swap.
  Correlation against JPEG #1 luma: **+0.826** at rot 0 / flip 0, next best +0.313.
- Sample values: file 1 temperatures 18.78 … 36.20 °C, mean 26.49; file 2 28.65 … 34.10 °C,
  mean 29.52. Aux field 0 … 253 in file 1 (254 distinct values), 66 … 253 in file 2. The aux
  meaning is still **[U]** (§12 E10).
- Compared with `PhotoCap` (§6): same measurement product and geometry, but the USB file keeps
  the `aux` plane and the settings metadata and drops the u16 raw plane.

Metadata block: 4063 bytes, of which only ~90 are non-zero, at fixed offsets.

| offset | content |
|---|---|
| 476, 478 | single bytes, unknown |
| 486–487 | two bytes, vary between files |
| 1479 | byte, unknown |
| 1631–1648 | small values, unknown |
| 1660 | f32 `1.4`, unknown |
| 1664, 1720 | measurement rectangles P1, P2: four u32 BE each (`x, y, w, h`), absent in some files |
| 3970 | ASCII `1.0`, immediately followed by `T2.0` (no separator) |
| 3980 | ASCII `1.1.9` (firmware), preceded by NUL padding |
| 3970–3990 | version window: search for the ASCII tokens rather than trusting a single offset; units differ by one byte |
| 3990 | ASCII `UTi260B+` (model) |
| 4000 | ASCII `0.95` (emissivity) |
| 4012 | 6 raw bytes (serial tail) followed by ASCII `0.25m` (distance) |
| 4028 | ASCII `55%RH` (humidity) |
| 4038 | ASCII ambient and flags: `25_0_0` in one variant, `25.0_0_0` in another — parse numerically, not by exact string |
| 4048 | ASCII `256x192` |
| 4058 | ASCII `0` (unit index) |

Treat every field as optional: files produced with different settings omit different blocks
(one of the two samples has no `q8` marker and no P1/P2 rectangles).

### 11.5 Other USB modes the firmware carries [V]

Only one gadget can bind to the UDC at a time. The module set in `/root/driver` is
`g_mass_storage`, `g_webcam`, `g_ss`, `g_ffs`, `gadgetfs`, `libcomposite`, `usb_f_fs`,
`usb_f_mass_storage`, `usb_f_ss`, `usb_f_uvc`. The device ships one script per mode in
`/mnt/UDISK/release`:

| script | what it actually binds |
|---|---|
| `sd_en.sh` | mass storage on `/dev/mmcblk0p10` — the shipped default |
| `uvc_en.sh` | `usb_f_ss` + `g_ss`: a bulk **sourcesink** gadget. Its UVC lines are commented out; the name is misleading |
| `adb_en.sh` | `usb_f_fs` + `g_ffs`, mounts functionfs, starts `adbd` |
| `mtp_en.sh` | calls `/bin/usbconfigfs.sh mtp`, which does not exist on this build |

Configfs is absent (`/sys/kernel/config` is empty), so MTP cannot work here. Note that
`adbd` is present on the device, so a debug bridge is one script away — but it needs a root
shell to enable, so it is a development tool, not a client feature.

UVC was tested directly by loading `g_webcam`, whose function driver (`usb_f_uvc`) and
`videobuf2_vmalloc` are built into the kernel; the `.ko` copies in `/root/driver` fail to
insert with "exports duplicate symbol", which is expected. The gadget binds and enumerates as
`35d4:260c` "Webcam gadget" advertising YUY2 240x320 at 30 fps and MJPEG 640x480 at 2 fps —
but the host cannot open it: the control channel never answers (`GET_INFO` times out,
`GET_DEF(PROBE)` unsupported, `GET_CUR(PROBE)` times out, probe fails with -5). The reason is
visible on the device: binding creates `/dev/video_usbcamera`, and only the camera's own
application writes frames there, and only when its webcam mode is selected. **A third-party
client cannot use the UVC path**, and even if it could, it would receive video, not
temperature.

Wi-Fi and USB coexist at the hardware level: with the gadget loaded manually, the AP stays up
and the host can read the volume while streaming over the network. In the shipped flow the
application tears down the AP when the user selects a USB mode, so in practice an import and a
live session do not overlap.

### 11.6 Ingest pipeline

USB is an **import path only**. Live view, palette, unit and `PhotoCap` stay on TCP 9527
(§5, §6). Nothing in the client may issue control commands over USB — there is no channel for
it, and pretending otherwise invites exactly the writes §11.2 forbids.

1. **Detect** by content first, transport second. Content: a mounted volume whose root holds
   `AUTORUN.INF` containing `UNIT UTi260B+` and a `DCIM/` directory. Transport: VID/PID
   `35d4:a4a5` with SCSI model `UTi260BPlus`. The content test survives VID changes and works
   on every platform; the transport test is a fast pre-filter, not the decision.
2. **Mount read-only.** Linux: `udisksctl mount -b <dev> --options ro`, or accept an existing
   mount only if it is read-only. macOS mounts NTFS read-only already. Windows mounts
   read-write and offers "scan and fix" — the client must not trigger writes and the UI must
   tell the user to decline any repair prompt.
3. **Watch** `DCIM/` for new names (Linux inotify, macOS FSEvents plus DiskArbitration,
   Windows directory-change notifications). Deduplicate by file name; the counter is monotonic
   but the device clock can jump, so the name is not a global key.
4. **Stabilise** before parsing: poll size and mtime until both are unchanged across two samples
   ~300 ms apart, then read the file in one pass. A capture is ~0.7 MB; do not stream it.
5. **Validate structurally** (§11.4): two JPEG SOI/EOI pairs, the `len2` word before *and* after
   JPEG #2, both equal to the JPEG #2 length, and
   `size == len(jpeg1) + 4 + len2 + 4 + 8*49152 + 4063`.
6. **Validate physically**: decode as 49 152 interleaved records and reject the file if any
   temperature falls outside a wide physical band (say −40 … 400 °C). A misaligned or planar
   read produces values in the ±1e30 range, so this check catches exactly the mistake that the
   structural check cannot.
7. **Copy out** to the host cache, then parse and index from the copy. Close the volume handle
   immediately — never hold it across a device power cycle or a mode change.
8. **Index** by SHA-256 of the copied file: store name, capture date from the name, firmware
   string, emissivity, distance, humidity, ambient, unit, min/max/mean temperature and the
   thumbnail from JPEG #1. Skip already-indexed hashes silently.
9. **Fail visibly but not destructively**: a rejected file is retried once after ~500 ms, then
   recorded as unreadable with its reason so a later pass can retry it. It must never appear in
   the gallery as a broken entry, and nothing on the camera is ever modified.

Cost per file: ~20 ms to read at measured throughput, single-digit milliseconds to parse. A
24-file library imports in about a second plus thumbnail decoding, which dominates — generate
thumbnails on the import worker, not in `update()`.

Reference crates (no USB or filesystem-specific dependency is required): `sysinfo` for mount
discovery, `notify` for file events, `image` or `jpeg-decoder` for JPEG #1, `sha2` for the
content key, `rusqlite` for the index.

UI rules that follow from the above:

- Show the import panel only while a volume is detected; display model and firmware taken from
  the file metadata, so the gallery stays self-describing with the camera disconnected.
- Warn that selecting a USB mode on the camera may drop its access point (§11.5). Do not run a
  live session and an import in parallel by design, even though the hardware allows it.
- Never offer "delete from camera", "rename on camera" or any other write action. Read-only is
  a design rule, not a user preference.

### 11.7 Paths that were evaluated and rejected

| path | blocker | evidence |
|---|---|---|
| UVC video out | gadget binds but never answers control requests; only the vendor app feeds `/dev/video_usbcamera`; video-only even then | §11.5 |
| bulk sourcesink (`g_ss`) | proprietary stream fed by the vendor app, format undocumented, no evidence it carries temperature | §11.5 |
| ADB over USB | requires a root shell to enable `adbd`, then root credentials on the host side | §11.5 |
| MTP | configfs absent from the kernel build and `/bin/usbconfigfs.sh` missing | §11.5 |
| direct NTFS block access | unnecessary; the OS mount is sufficient, and a bespoke writer risks the corruption described in §11.2 | §11.2 |

USB therefore contributes exactly one capability to the client — offline radiometric import —
and the whole integration is file handling plus validation.

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
| E20 | does the live frame ever carry a temperature plane? | yes — every 11th frame, 311 835 bytes total, f32 192x256 plane (70 frames measured: indices 3, 14, 25, 36, 47, 58, 69) |
| E22 | unit byte meaning and recovery from a "stuck" state | resolved: the byte is a bit field — bit 1 = Kelvin, bit 2 = independent flag. `+0+0` = Celsius, `+0+1` = Kelvin, `+1+*` touches only bit 2. Wire key temperatures never change unit. No reboot needed once the fields are understood |

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
| E12 | `scaleUpdata`: layout is decoded, but how to reach a manual scale mode remotely is not | sent the decoded 42-byte frame in the device's auto mode — accepted without crashing but with no effect, because automatic bounds overwrite it; `scaleChange+<n>` (n = 0, 2, 3, 4) did not change the reported mode either. Next: find the trigger that puts the scale into a manual mode (possibly only reachable through the on-screen menu), then repeat the write | low once the mode is reached |
| E13 | `PhotoCap` section 4 (8 zero bytes) | compare across many captures, vary settings | none |
| E14 | `HeartBack` interval and whether omission drops the connection | hold a socket idle with and without periodic `HeartBack`, watch for teardown | none |
| E15 | Argument forms for `MenuHide`, `Shutter`, `CalibraCam` | send with plausible single integers, observe OSD/state; `CalibraCam` is risky, skip unless needed | `CalibraCam` may disturb calibration |
| E16 | Exact reply shape when `PhotoCap` is rejected as busy | **resolved**: `{UTi_Photo}` + u32 `17` + CRLF, zero-length payload (17 bytes); the state is sticky for at least 30 s while `vFrame` keeps working (§6.2) |
| E23 | Which exact concurrency pattern kills the application — the second request, the second connection, or the combination? | bisect with one variable per trial, power cycle between trials | **kills the application every time it goes wrong**; needs a power cycle, not a reconnect |
| E24 | What does bit 2 of the unit byte (`0x4`) control? It survives reboot and is toggled by `measUnit+1+0` / `+1+1` | **partially resolved**: flipping it changed nothing in `init` (ambient, alarm, distance, colour-scale block all identical) and nothing structured in the live video (difference scattered, max 23 ADU, no text-shaped region). Remaining candidate: a setting that only surfaces in reports — compare the USB metadata distance string (`0.25m` vs an imperial variant) with bit 2 set | none |
| E17 | Does the on-device shutter produce the same container as the USB files? | **resolved**: yes, same records and metadata, but the secondary JPEG is optional — the shutter wrote a thermal-only file (one JPEG, `len2 = -1`) while the visible pipeline was down (§11.4) |
| E18 | Which `[setusb] usb=` value selects which gadget mode | change the value in `IniFormat.ini` and observe which module binds | may drop the AP; power cycle recovers |
| E19 | Does the application feed `/dev/video_usbcamera` in its webcam mode? | select the webcam mode in the device UI, enumerate on a host, try to open the stream | none; revert by reselecting the storage mode |

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
   token, missing CRLF, or a payload length that is neither 115 212 (base: 115 200 pixel
   block + 12 key-temperature bytes) nor 311 820 (base + 196 608-byte f32 plane).
7. No credential or IP is hardcoded; IP resolves from the default gateway with manual
   override. The one hardcoded string allowed is the USB volume marker
   (`label=UNIT UTi260B+`, §11.6) used for device discovery — it identifies a volume, not a
   protocol, and must be treated as configurable because other models may carry a different
   marker.
8. Raw `.yuv` + `.bin` fixture replay mode (`--replay`) drives the UI identically for
   regression tests without the camera.
9. Measurement overlay: clicking the display maps to the capture plane by
   `plane_x = display_x / 1.25`, `plane_y = display_y / 1.25` (192x256 plane), with no
   rotation or flip; a self-test asserts the documented luma correlation ≥ 0.8 against a
   stored PhotoCap fixture.
10. Command channel is a typed whitelist. No code path can emit an argument the device has
    not been verified to accept; unit tests assert the exact bytes for every command,
    including the binary `pipRectMove` frame.
11. Palette and unit changes are confirmed before the UI state changes: palette by reading
    the next frame and comparing its signature **loosely** against the one-scene references
    in §3.1 (a shift in mean hue/saturation, not an exact match), unit by re-reading `init`
    and trusting the reported byte. A failed confirmation surfaces an error instead of a
    silent no-op.
12. Exactly one `vFrame` in flight at all times; a dropped extra request must not desync the
    reader. The client reports achieved fps, not requested fps.
13. `PhotoCap` is serialised by the client, and the client never relies on a busy reply —
    there is none. No second capture, no second connection and no other request is sent while
    a capture is in flight (§6.2); a capture that times out marks the device failed rather than
    being retried in place.
14. Idle sessions send `HeartBack`; the client does not assume a keep-alive reply exists.
15. USB import only ever opens files read-only; the client never writes, renames or repairs
    anything on the camera's volume.
16. A USB file is accepted only when both length words and the total-size equation agree; any
    mismatch is reported as a retryable read error, never as a corrupt image.
17. Imported files are keyed by content hash, so re-importing the same card does not duplicate
    entries, and captured parameters (emissivity, distance, humidity, unit, firmware) are
    preserved with the image.
18. Import runs on its own worker and communicates by channel: a volume that disappears
    mid-import cannot stall the render loop or the network thread.
19. Decoded temperatures are sanity-bounded before display; an out-of-band value marks the file
    unreadable rather than rendering a false measurement.
20. The shutter/FFC cycle is handled explicitly (§5.5): frames around it are not used for
    measurement, a detected step re-baselines the session instead of being reported as a scene
    change, and the client can request an FFC on demand via `UTi_cmdsys:Shutter` before a
    measurement series.

---

## 14. Fixtures produced so far

| path | content |
|---|---|
| `analysis/capture/live_clean.yuv` | one full 115 200-byte YUV420 live frame |
| `analysis/capture/live_240x320_uv.png` | decoded reference image (BT.601, U then V) |
| `analysis/capture/frame_rgb.bin` | earlier raw frame, same layout |
| `analysis/capture/session1/photocap_raw.bin` | full 563 661-byte `PhotoCap` reply, header included |
| `analysis/capture/session1/live_extended_f32plane.bin` | full 311 835-byte extended `vFrame` with the 192x256 f32 plane (temps 30.17–34.30 °C, mean 31.44) |
| `analysis/capture/session1/temp_256x192.f32` | temperature plane, f32 LE, 192x256 row-major |
| `analysis/capture/session1/y16_256x192.u16` | raw plane, u16 LE, 192x256 |
| `analysis/capture/session1/shot_240x320.jpg` | section 0, JPEG with OSD |
| `analysis/capture/session1/main_480x640.jpg` | section 2, clean JPEG |
| `analysis/capture/session1/temp_map_192x256.png` | false-colour render of the temperature plane |
| `analysis/capture/session1/*.log` | palette and unit probe transcripts |
| `analysis/device/usb_samples/*.jpg` | two radiometric USB files, container-verified end to end |
| `analysis/device/usb_samples/photocap_live.bin` | full 584 574-byte `PhotoCap` reply from the same session |
| `analysis/device/usb_recon*.log`, `usb_uvc_switch.log` | USB mode, gadget and mount recon transcripts |
| `analysis/*.json` | REA/Ghidra dossiers for the Guide-SDK families (not needed for this client) |
| `analysis/native/*.json` | reverse-engineering dossiers of the device binaries; see Appendix A for the claim-to-source mapping |
| `analysis/device/fwbackup/` | verified partition/GPT images for device restore (not protocol fixtures) |

Still missing: raw `init` reply bytes, a full `vFrame` message with header, and any gallery
reply. Those three complete the fixture set.

These paths are **not** in this repository: the fixtures live in the analysis archive of the
reverse-engineering workbench (same layout, `analysis/…`), because they are captures from the
author's device rather than specification material. Acceptance criteria 3 and 9 are therefore
verified externally against that archive; a clean-room implementer can substitute their own
capture, since every criterion is phrased in terms of the lengths and correlations documented
above rather than a specific file.

---

## 15. Device coverage

Everything above was verified on a UTi260B+ (DEV_TYPE 112, firmware 1.1.9). The vendor client
serves a family of models over the same transport, and the per-model protocol classes can be
compared mechanically: normalise each class (strip comments, package paths, log levels) and
diff against this model's. Zero differences means the same wire format; only model identity and
geometry change. Method and raw diffs: `DEVICE-SUPPORT-MATRIX.md` and
`analysis/device/device_matrix/`.

### 15.1 What actually differs between models [I]

Two things, and neither is the message framing:

| aspect | this device | most other models |
|---|---|---|
| live pixel block | `w*h*3/2` (I420) | `w*h*3` (RGB888) |
| live temperature plane | f32 plane on every 11th frame; no u16 plane observed | optional, length-selected |

The live reply may append a temperature plane after the three key-temperature floats:

```text
token {UTi_img} 9 | u32 length 4 | pixels | 3 x f32 BE key temps 12 | [plane] | CRLF 2
plane, 16-bit variant: raw_w * raw_h * 2 bytes
plane, 32-bit variant: raw_w * raw_h * 4 bytes
```

There is no flag for it: the client selects by payload length. This unit alternates — ten
frames of 115 227 bytes (base), then one of 311 835 bytes (base + f32 plane) — so live
per-pixel temperature **is** available here at roughly 2 Hz (measured over 70 requests,
planes at indices 3, 14, 25, 36, 47, 58, 69). The u16 variant is permitted by the protocol but
has not been observed on this firmware.

Geometry is also not transmitted — the vendor client hardcodes it per model. A client should
recover it once from a `PhotoCap`: section 1 length is `raw_w*raw_h*4`, and the JPEG headers in
sections 0 and 2 give the display size. That removes the need for a per-model table.

### 15.2 Tier 1 — wire-identical, name and geometry only [I]

Zero differences in `init` parsing, `PhotoCap` parsing, socket handling and command strings
against this model.

| model | DEV_TYPE | raw w x h | display w x h |
|---|---|---|---|
| UTi260B+ | 112 | 192 x 256 | 240 x 320 |
| UTi720B | 112001 | 192 x 256 | 240 x 320 |
| UTi320V+ | 106 | 320 x 240 | 640 x 480 |
| UTi165B+ | 111 | 120 x 160 | 240 x 320 |
| UTi716B | 111001 | 120 x 160 | 240 x 320 |
| UTi260V | 107 | 256 x 192 | 640 x 480 |

### 15.3 Tier 2 — same protocol, differing `init`/`PhotoCap` fields [I]

Same transport and command vocabulary; extra or reordered fields. One capture per model settles
each.

| model | DEV_TYPE | raw w x h | notes |
|---|---|---|---|
| UTi260E | 103 | 256 x 192 (older client: 260 x 195) | `init` layout differs; geometry is firmware-dependent |
| UTi320E | 101 | 320 x 240 | `init` layout differs |
| UTi384H, UTi384H JX003, UTi740H | 102 / 110 / 102001 | 384 x 288 | `init` differs, 10 command strings differ; live port 9527 or 9528 — probe both |
| UTi260T, UTi720T | 109 / 109001 | 256 x 192 | `PhotoCap` differs most of the family |
| UTi720E / UTi730E, UTi720V / UTi730V | 103001 / 101001, 107001 / 106001 | as their base class | reuse the E/V classes |

### 15.4 Tier 3 — different transport, outside this document

| model | transport |
|---|---|
| UTi640Q | RTSP on 8086, records on 8087; raw 640 x 512 |
| UTi384G, UTi256G | Guide SDK HTTP on 23021 plus RTSP on 8554 — no `UTi_cmdsys` command set |

See §1 and §10: they are documented here only as a separate family that a client must not
confuse with this one.

### 15.5 Consequences for a client

Parse by length arithmetic rather than by model. Four numbers have to be known before the
first frame can be parsed — `display_w`, `display_h`, `raw_w`, `raw_h` — and there are two
sources for them:

1. the model table in §15.2/§15.3 (works from the first byte on the wire), or
2. one `PhotoCap` reply, which yields `raw_w*raw_h` from section 1 and the display size from
   the JPEG headers. Until that reply arrives, use the table; after it, trust the measurement.

Algorithm, with `body` = message minus token, length field and CRLF:

```text
body_len = total - token_len - 4 - 2

candidates = [ (display_w * display_h * 3 / 2, "I420"),
               (display_w * display_h * 3,     "RGB888") ]

for pix_len, fmt in candidates:
    tail = body_len - 12 - pix_len
    if tail == 0:                       accept(fmt, plane = none)
    if tail == raw_w * raw_h * 2:       accept(fmt, plane = u16)
    if tail == raw_w * raw_h * 4:       accept(fmt, plane = f32)
reject()                                # no candidate fits: framing violation
```

Definitions for the accepted branches:

| item | definition |
|---|---|
| I420 | planar Y then U then V, each plane `display_w * display_h` subsampled as usual; total `3/2` bytes per pixel; portrait orientation, `U` before `V` |
| RGB888 | interleaved R, G, B, row stride `display_w * 3`, no padding, top-left origin |
| u16 plane | little-endian, row stride `raw_w`, `raw_h` rows, same orientation as the f32 plane; a device-scaled sensor value, **not** degrees Celsius — see §6.1 for why it cannot be converted by a fixed formula |
| f32 plane | little-endian degrees Celsius, row stride `raw_w`, `raw_h` rows, ready to display through the mapping in §6.1 |

A client that only supports the reference model may stop at I420 plus the f32 tail, but it
must still reject unknown lengths cleanly rather than resynchronising (§7).

Supporting a tier 1 model is not "no code change": the geometry differs for every model, and
the live pixel format may be RGB888 instead of I420. It is *no protocol change* — the messages,
the command set and the `PhotoCap` section sequence are identical. Tier 2 additionally needs
confirmation that the fields this client reads have not moved in `init`. Anything this document
marks **[U]** — the u16 live tail, per-model plane periodicity — depends on the firmware and
must be measured per unit.

---

## Appendix A — Evidence provenance (non-normative)

This appendix exists so a reviewer can audit where each claim came from. It is **not**
implementation guidance: it names vendor components only to make the evidence traceable, and
no vendor code, pseudocode or decompiled fragment appears here or anywhere in this document.
Nothing above requires reading vendor software to implement.

| id | claim | how established | source artifact |
|---|---|---|---|
| A1 | AP SSID/passphrase, camera IP, open ports, one process serving 9527+9528 | live scan, DHCP lease, `netstat` over the device shell, `hostapd.conf` | `DEVICE-PROFILE.md`, `analysis/device/recon*.log` |
| A2 | complete command surface, `UTi_cmd` gating, argument splitting on `+` | static read of the request handler in the device application; corroborated live for `colorChange` and `measUnit` | vendor component: the application binary's request dispatcher (symbol `RtcpImage::ReadDataFromSocket`, offset `0x4d60c0`); live logs in `analysis/capture/session1/` |
| A3 | `vFrame` is a producer trigger; one frame in flight; extras latched then dropped; `HeartBack` sets a keep-alive flag | same handler (frame semaphore release, pending flag, keep-alive flag at object offset `0x1138`); matches the measured 37–47 ms polling behaviour | same as A2 |
| A4 | `PhotoCap` is rejected while a capture is running, ~50 ms settle before capture | same handler (capture guard, error path, log string `PhotoCap error!`) | same as A2 |
| A5 | radiometry: NUC table → Kelvin×16 → °C; coefficient input bounds; fixed-point transmittance ratio | static read of the radiometry library shipped on the device | vendor component: `libirtemp.so`, symbols `calc_temperature_with_nuc_t_table_len` `0x1104a0`, `calculate_KE_and_BE` `0x10c6e0`, `calculate_tau` `0x10d4c4` |
| A6 | device measurement primitives are point/line/rectangle with bounds checks | static read of the same library | `get_point_temp` `0x104528`, `get_line_temp`, `get_rect_temp` |
| A7 | gallery channel is `#`-prefixed text; serving side waits for a `#` byte; non-commands answer `STATUS:Ready` | static read of the gallery handlers plus live probe of port 9528 | `GetPhotoDir` `0x607498`, `ReadDataFromSocket` `0x4ed4c8`; `analysis/capture/gallery/` |
| A8 | frame layout, 240x320 YUV420, geometry 192x256, orientation, key-temp encoding | live captures, decoded and cross-checked numerically | `analysis/capture/`, `analysis/capture/session1/` |
| A9 | palette indices, unit encoding, emissivity/humidity/distance/alarm values, laser coordinates | live probes compared against the device's own settings files | `analysis/capture/session1/*.log`, `analysis/device/release/` INIs |
| A10 | platform, kernel, partitions, service list, calibration table | read-only shell recon on the owner's device | `DEVICE-PROFILE.md`, `analysis/device/recon{1..6}*.log` |
| A11 | USB enumeration, gadget modes, exported block device, throughput, container layout, mode-script inventory | host-side descriptor and throughput measurement plus read-only device shell recon; mode switch performed and reverted | `analysis/device/usb_recon*.log`, `analysis/device/usb_uvc_switch.log`, `analysis/device/usb_samples/` |
| A12 | device coverage per model: protocol identity, geometry, live pixel format | normalised per-model diff of the vendor client's protocol classes across two client generations | `DEVICE-SUPPORT-MATRIX.md`, `analysis/device/device_matrix/` |

Detailed reverse-engineering notes, binaries and the restore kit are kept outside this
repository, alongside the working analysis directory.
