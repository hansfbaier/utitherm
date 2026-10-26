# uti260b-viewer

Independent Rust client for the UNI-T **UTi260B+** thermal imager — egui GUI
plus an automation CLI — implemented from
[`UTi260B-plus-protocol-spec.md`](UTi260B-plus-protocol-spec.md).
No vendor code, libraries, credentials or fixed IP addresses are involved.

**CLI documentation:** open [`docs/cli.html`](docs/cli.html) in a browser for the
full command reference, JSON schemas and capture file formats.

Scope is the TCP 9527 protocol: `init`, the live `vFrame` stream and the
`PhotoCap` full-resolution capture with its 192×256 f32 temperature plane.
Commands are a typed whitelist because malformed arguments kill the device
daemon and require a power cycle (spec §3).

## Build and run

```sh
cargo build --release                       # GUI + CLI binaries
cargo run --release --bin uti260b-viewer    # live mode, IP from default gateway
cargo run --release --bin uti260b-viewer -- --ip 192.168.27.10
cargo run --release --bin uti260b-cli -- info
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
- Accepts both live-frame payloads: the base 115 212-byte reply and the extended
  311 820-byte reply carrying a 192×256 f32 temperature plane. This firmware
  appends the plane to every 11th frame (~0.6 s at 20 fps).
- Shows the full `init` state, marking inferred `[I]` and unknown `[U]` fields,
  and surfaces unit changes as confirmed only after re-reading `init`.
- **PhotoCap** button (user-initiated, ~3 s, writes a file on the camera SD card)
  fetches a full-resolution capture and shows its statistics, section sizes and
  the 192×256 geometry. Live frames also carry a lower-rate temperature plane.
- Clicking the image samples the newest capture plane, or the most recent live
  plane when no capture exists, through the documented mapping
  `plane = display / 1.25` (no rotation, no flip); it draws a small crosshair
  and shows the temperature in °C.
- Palette changes (`colorChange+0..7`) are confirmed against following frames
  using the reference mean-RGB/saturation signatures; a mismatch is surfaced
  instead of silently accepted.
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

| file                     | role                                                               |
| ------------------------ | ------------------------------------------------------------------ |
| `src/protocol.rs`        | framing, typed command whitelist, `init`/`vFrame` parsing          |
| `src/yuv.rs`             | BT.601 full-range YUV 4:2:0 → RGBA                                 |
| `src/capture.rs`         | `PhotoCap` sections, temperature plane, mapping, luma self-test    |
| `src/net.rs`             | worker, state machine, command queue, stats, events                |
| `src/replay.rs`          | fixture replay through the same parsers/UI                         |
| `src/config.rs`          | config file, gateway discovery                                     |
| `src/app.rs`             | egui front end, palette/unit confirmation, measurement overlay     |
| `src/cli.rs`             | automation subcommands, JSON output, file writers                  |
| `src/palette.rs`         | frame signature and nearest-palette helper                         |
| `src/fixtures.rs`        | synthetic `init`/`vFrame`/`PhotoCap` messages for tests and replay |
| `src/lib.rs`             | shared library used by both binaries                               |
| `src/bin/uti260b-cli.rs` | CLI entry point                                                    |

## Automation CLI

`uti260b-cli` is the non-GUI interface. It uses the same worker, framing,
validation, reconnect logic and typed command whitelist as the GUI. One process
per invocation; exit code is non-zero on failure. Diagnostics go to stderr.

```sh
uti260b-cli info
uti260b-cli --json info
uti260b-cli frame --png frame.png --yuv frame.yuv --plane temp.f32 --require-plane
uti260b-cli frames --count 10 --dir frames --png --yuv --plane
uti260b-cli capture --dir capture --raw capture/raw.bin
uti260b-cli measure --display 121,161
uti260b-cli measure --temp-file capture/temperature.f32 --display 121,161
uti260b-cli measure --capture-file capture/raw.bin --plane 96,128
uti260b-cli palette 3
uti260b-cli unit celsius
uti260b-cli watch --frames 100 --json --sample 121,161
```

Global options: `--ip <addr|auto>`, `--port`, `--config`, `--fps`,
`--connect-timeout-ms`, `--read-timeout-ms`, `--timeout-ms`, `--json`,
`--quiet`.

| command   | purpose                                                                          |
| --------- | -------------------------------------------------------------------------------- |
| `info`    | init state and unit byte                                                         |
| `frame`   | one live frame; optionally saves PNG, YUV, plane                                 |
| `frames`  | N frames into a directory                                                        |
| `capture` | PhotoCap sections: shot/vis JPEG, temperature f32, y16, tail, optional raw reply |
| `measure` | temperature at display or plane coordinates; live, saved plane or saved capture  |
| `palette` | send `colorChange+0..7`, then check following frames                             |
| `unit`    | send a verified `measUnit` pair, settle, re-init and report the unit byte        |
| `watch`   | one line per frame; JSON lines with `--json`                                     |

- `measure` uses the live plane by default (waits for an extended frame). The
  mapping is `plane = display / 1.25`, plane 192×256.
- `frames` saves PNG+YUV by default when no format flag is given; `--plane` alone
  saves planes only.
- JSON output is one object per command, except `watch`, which prints one object
  per frame.
- `capture` writes a file on the camera SD card; expect ~3 s.
- The camera accepts a single client at a time. Back-to-back CLI invocations may
  need a short gap; the worker retries within `--timeout-ms`.
- Only verified commands are reachable: `init`, `vFrame`, `PhotoCap`,
  `colorChange+0..7`, and `measUnit+1+0|+1+1|+0+1`.

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

Live `vFrame` replies on this firmware come in two sizes: 115 227 B total
(YUV + 3 key temperatures) and, on every 11th request, 311 835 B total with the
same fields plus the 196 608-byte f32 plane appended after the key temperatures.

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
against a fake camera (init, reconnect, no-reply commands, capture, extended
live-plane frames), palette signature matching, capture round-trips, CLI argument
parsing and PNG/YUV export. The suite has 54 tests.

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
- Firmware 1.1.9 sends a live temperature plane on every 11th `vFrame`, contrary
  to spec §5.2/§10. Payloads are accepted in both forms.
- Frame pacing is scheduled from the request start. Scheduling from the reply
  arrival capped throughput at `RTT + interval` (~11 fps); the observed rate is
  now ~18 fps at a 20 fps target.
- `measUnit+0+1` switched the camera to Kelvin, but `init` first reported the old
  unit and after `measUnit+1+0` reported unit byte `2` while the OSD stayed in K.
  The GUI and CLI report the raw byte; a power cycle or camera-menu reset is the
  safe recovery. Only verified pairs are ever sent.

## Known limits

- Live measurement updates only every 11th frame (one plane per ~0.6 s at
  20 fps); `PhotoCap` gives a full-resolution plane and JPEGs.
- Unit and palette semantics differ from the spec table on the tested firmware;
  both are reported as detected/reported values, not silently trusted.
- Palette cannot be recoloured client-side; the device must change it.
- OSD cannot be hidden from this client (spec §12 E6); the sampling overlay is
  drawn on top of the image, outside the OSD region only by convention.
- Gallery port 9528 and the USB radiometric container are not decoded.
- `pipRectMove`, `scaleUpdata` and the unverified command arguments are not sent.
