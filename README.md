# uti260b-viewer

Independent Rust + egui client for the UNI-T **UTi260B+** thermal imager,
implemented from [`UTi260B-plus-protocol-spec.md`](UTi260B-plus-protocol-spec.md).
No vendor code, libraries, credentials or fixed IP addresses are involved.

Scope is the TCP 9527 protocol: `init`, the live `vFrame` stream and the
`PhotoCap` full-resolution capture with its 192×256 f32 temperature plane.
Commands are a typed whitelist because malformed arguments kill the device
daemon and require a power cycle (spec §3).

## Build and run

```sh
cargo run --release                 # live mode, IP from default gateway
cargo run --release -- --ip 192.168.27.10
cargo run --release -- --make-fixtures fixtures --replay fixtures
cargo run --release -- --verify-capture analysis/capture/session1/photocap_raw.bin
```

Join the camera's Wi-Fi AP (`UTi260B+`) first, then press **Connect**.
`--ip auto` (the default) reads the host default gateway — while joined to the
camera AP the camera is the gateway.

Space toggles connecting when a text field does not have focus.

## What the UI does

- Streams frames into a single `TextureHandle`, default integer scaling computed
  in physical pixels so the burned-in OSD stays crisp at fractional DPI;
  nearest-neighbour filtering at integer scale, linear for fractional fit.
  Aspect ratio 3:4.
- Shows the three per-frame header temperatures as unlabelled readings with the
  device unit. They are **not** max/min/centre of the measurement plane
  (spec §5.2, E4).
- Shows the full `init` state, marking inferred `[I]` and unknown `[U]` fields,
  and surfaces unit changes as confirmed only after re-reading `init`.
- **PhotoCap** button (user-initiated, ~3 s, writes a file on the camera SD card)
  fetches the only per-pixel temperature source and shows its statistics,
  section sizes and the 192×256 geometry.
- Clicking the image samples the plane through the documented mapping
  `plane = display / 1.25` (no rotation, no flip), draws a small crosshair and
  shows the temperature in °C.
- Palette changes (`colorChange+0..7`) are confirmed against the second frame
  that follows the request using the reference mean-RGB/saturation signatures;
  a mismatch is surfaced instead of silently accepted.
- Status bar: connection state, RTT, achieved fps, frame count, frames dropped
  because the UI did not paint them, stale-frame age, and the three version
  strings.
- **Export PNG + YUV** writes the current live frame to `./captures/`.
  The capture panel exports the temperature plane (f32 LE °C) and the raw u16
  plane.

## Architecture

```text
[ net worker thread ]  blocking TcpStream, session state machine, reconnect backoff
                       write command → read token/length/payload/CRLF → validate
                       YUV→RGBA and PhotoCap decoding → ArcSwap / event stream
[ UI thread ]          drain events, take newest frame, upload texture, paint
```

- Framing errors (wrong token, length mismatch, missing CRLF, bad payload size)
  drop the connection and re-run `init`; the stream is never resynchronised by
  scanning (spec §7).
- No frame queue: `ArcSwap` overwrites unpainted frames. The UI reports the gap
  as dropped frames.
- Conversion and capture parsing run on the worker thread; the UI callback never
  touches the planes.
- Socket timeouts (2 s default), `TCP_NODELAY` and `SO_KEEPALIVE` are set.
  `PhotoCap` temporarily raises the read timeout to 15 s because the device needs
  ~3.3 s (spec §6).
- Shutdown joins the worker and unblocks a pending read with `shutdown(Both)`.

### Modules

| file              | role                                                               |
| ----------------- | ------------------------------------------------------------------ |
| `src/protocol.rs` | framing, typed command whitelist, `init`/`vFrame` parsing          |
| `src/yuv.rs`      | BT.601 full-range YUV 4:2:0 → RGBA                                 |
| `src/capture.rs`  | `PhotoCap` sections, temperature plane, mapping, luma self-test    |
| `src/net.rs`      | worker, state machine, command queue, stats, events                |
| `src/replay.rs`   | fixture replay through the same parsers/UI                         |
| `src/config.rs`   | config file, gateway discovery                                     |
| `src/app.rs`      | egui front end, palette/unit confirmation, measurement overlay     |
| `src/fixtures.rs` | synthetic `init`/`vFrame`/`PhotoCap` messages for tests and replay |

## Commands (typed whitelist)

The device exits its 9527/9528 daemon on a malformed command and does not
restart it (observed 2026-10-24). There is therefore **no free-form command
input**: `protocol::Command` is the only way to emit bytes, and its parameterised
variants are enumerated or range checked.

| command  | form                                      | verified       |
| -------- | ----------------------------------------- | -------------- |
| init     | `UTi_cmdsys:init`                         | yes            |
| vFrame   | `UTi_cmdsys:vFrame`                       | yes            |
| PhotoCap | `UTi_cmdsys:PhotoCap`                     | yes            |
| palette  | `UTi_cmdsys:colorChange+0..7`             | yes (no reply) |
| unit     | `UTi_cmdsys:measUnit+1+0`, `+1+1`, `+0+1` | yes (no reply) |

`pipRectMove` is documented as a binary frame (command name + `u32 BE` total
length 38 + four `u16 LE` values + `"/n/a"`); the encoder is unit-tested but not
reachable from the UI because the rectangle semantics are unverified. All other
named commands use arguments that have not been verified, so they are not
exposed (spec §3 safety rule, acceptance §13.10).

## Measurement and PhotoCap

`PhotoCap` reply sections (spec §6):

| # | length  | content                 | geometry            |
| - | ------- | ----------------------- | ------------------- |
| 0 | varies  | JPEG with OSD           | 240×320             |
| 1 | 196 608 | f32 LE °C               | 192 wide × 256 tall |
| 2 | varies  | JPEG, clean image       | 480×640             |
| 3 | 98 304  | u16 LE raw sensor plane | 192×256             |
| 4 | 8       | unknown, zero           | —                   |

The plane row stride is 192. Rendering it as 256 wide tiles four bands; the
self-test and overlay use 192. Display coordinates map as `plane = display /
1.25`, with no rotation or flip. The u16 plane is not linear in temperature;
it is kept for diagnostics only.

`--verify-capture <file>` parses a stored reply (framed or bare payload), decodes
section 0 as JPEG, and reports the Pearson correlation between its luma and the
temperature plane sampled through the documented mapping. It fails if the
correlation is below the documented 0.8. This needs a real capture; the synthetic
`photocap.bin` fixture uses JPEG marker stubs and will not decode.

## Configuration

`./uti260b.toml` or `~/.config/uti260b/config.toml` (see
`uti260b.toml.example`):

```toml
[camera]
ip = "auto"
live_port = 9527
connect_timeout_ms = 2000
read_timeout_ms = 2000
target_fps = 20
out_dir = "captures"
```

CLI flags override the file.

## Tests and replay

```sh
cargo test
```

Tests cover framing rejection (wrong token, impossible length, missing CRLF,
truncated body, wrong payload size, non-finite floats), `init` field offsets,
big-endian key temperatures, BT.601 conversion against a reference formula,
exact bytes for every whitelisted command including the binary `pipRectMove`
frame, `PhotoCap` section parsing and the 192-stride mapping/correlation,
fixture round-trips, config parsing, replay loading, an end-to-end worker test
against a fake camera (init, reconnect, no-reply commands, capture) and PNG/YUV
export.

`--make-fixtures <dir>` writes a synthetic fixture set:

| file             | content                                                         |
| ---------------- | --------------------------------------------------------------- |
| `init.bin`       | 160 B framed `init` reply with documented values                |
| `vframe.bin`     | 115 227 B framed `vFrame` reply                                 |
| `live_clean.yuv` | 115 200 B raw YUV 4:2:0 block                                   |
| `photocap.bin`   | 563 661 B framed `PhotoCap` reply (JPEG stubs, synthetic plane) |

`--replay` accepts a file or directory of framed messages (concatenated messages
allowed) and raw YUV blocks. A `PhotoCap` message in the replay set becomes the
measurement plane, so the overlay and mapping can be exercised without hardware
(spec §13.8).

## Spec notes

- The spec says the `init` and `vFrame` requests are both 15 bytes; the literal
  strings are 15 and 17 bytes. `PhotoCap` is 19 bytes, not the 15 stated in §6.
  Requests are sent as the literal command string, so nothing depends on those
  numbers.
- The `init` colour-scale block decodes to 50.83 and 31.87 from the captured
  bytes (`4d f8 fe 41`), while the spec table prints 32.00 for the second value.
  The UI shows the decoded values and marks the block `[U]`.
- Palette confirmation matches the reference mean-RGB/saturation signatures
  (§3.1) against a decoded frame, which includes the OSD. It is a heuristic and
  is labelled as detected, not measured.
- The synthetic `photocap.bin` is for framing/replay only; `--verify-capture` is
  intended for the real `analysis/capture/session1/photocap_raw.bin` fixture.

## Known limits

- Live frames carry only three unlabelled temperatures; arbitrary-point
  measurement requires `PhotoCap`.
- Palette cannot be recoloured client-side; the device must change it.
- OSD cannot be hidden from this client (spec §12 E6); the sampling overlay is
  drawn on top of the image, outside the OSD region only by convention.
- Gallery port 9528 and the USB radiometric container are not decoded.
- `pipRectMove`, `scaleUpdata` and the unverified command arguments are not sent.
