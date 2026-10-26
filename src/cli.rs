//! Automation CLI for the UTi260B+ camera.
//!
//! Reuses the same worker, framing, command whitelist and capture decoder as the
//! GUI. Output is human-readable by default and single-line/structured JSON with
//! `--json`, so scripts can consume it.

use std::fs;
use std::io::BufWriter;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::capture::{self, Capture};
use crate::config::{CameraAddr, Config};
use crate::frame::LiveFrame;
use crate::net::{ConnState, Event, Level, NetStats, StreamConfig, Worker};
use crate::palette::{PALETTE_MATCH_DISTANCE, equivalent, frame_signature, nearest_palette};
use crate::protocol::{self, Command, InitReply, Palette, UnitCommand};
use crate::usb::{self, IndexEntry};

const DEFAULT_OPERATION_TIMEOUT_MS: u64 = 20_000;
const CAPTURE_TIMEOUT_MS: u64 = 30_000;

#[derive(Parser, Debug)]
#[command(
    name = "utitherm-cli",
    version,
    about = "Automation CLI for UNI-T UTi_cmdsys thermal imagers (TCP 9527)",
    long_about = "Non-GUI camera client for scripts. Commands are sent from the verified \
                  whitelist only; the device daemon exits on malformed arguments."
)]
pub struct Cli {
    /// Camera address, or "auto" for the default gateway of the camera AP
    #[arg(long, global = true, value_name = "IPV4|auto")]
    ip: Option<String>,
    /// Live-view TCP port
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Config file (default ./utitherm.toml, then ~/.config/utitherm/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// TCP connect timeout in milliseconds
    #[arg(long, global = true)]
    connect_timeout_ms: Option<u64>,
    /// Socket read/write timeout in milliseconds
    #[arg(long, global = true)]
    read_timeout_ms: Option<u64>,
    /// Frame-loop target rate
    #[arg(long, global = true)]
    fps: Option<u32>,
    /// Machine-readable JSON on stdout
    #[arg(long, global = true)]
    json: bool,
    /// Suppress connection diagnostics on stderr
    #[arg(long, global = true)]
    quiet: bool,
    /// Per-command timeout in milliseconds
    #[arg(long, global = true, default_value_t = DEFAULT_OPERATION_TIMEOUT_MS)]
    timeout_ms: u64,
    #[command(subcommand)]
    command: CommandLine,
}

#[derive(Subcommand, Debug)]
enum CommandLine {
    /// Read init and print device state
    Info,
    /// Fetch one live frame
    Frame(FrameArgs),
    /// Fetch several live frames and save them
    Frames(FramesArgs),
    /// Run PhotoCap and save all payload sections
    Capture(CaptureArgs),
    /// Sample a temperature from live, saved-plane, capture or USB data
    Measure(MeasureArgs),
    /// Change the palette and check the following frames
    Palette(PaletteArgs),
    /// Change the temperature unit and confirm with a fresh init
    Unit(UnitArgs),
    /// Request a manual flat-field correction / shutter
    Shutter,
    /// Stream one JSON object (or line) per live frame
    Watch(WatchArgs),
    /// Import radiometric files from a read-only USB volume
    Import(ImportArgs),
}

#[derive(clap::Args, Debug)]
struct FrameArgs {
    /// Save the decoded frame as PNG
    #[arg(long)]
    png: Option<PathBuf>,
    /// Save the raw YUV 4:2:0 block
    #[arg(long)]
    yuv: Option<PathBuf>,
    /// Save the live temperature plane (f32 LE) when present
    #[arg(long)]
    plane: Option<PathBuf>,
    /// Wait for an extended frame that carries the live plane
    #[arg(long)]
    require_plane: bool,
    /// Frames to inspect while waiting for the plane
    #[arg(long, default_value_t = 33)]
    max_frames: u32,
}

#[derive(clap::Args, Debug)]
struct FramesArgs {
    /// Number of frames to save
    #[arg(long, default_value_t = 1)]
    count: u32,
    /// Output directory
    #[arg(long, default_value = "frames")]
    dir: PathBuf,
    /// Save PNG frames
    #[arg(long)]
    png: bool,
    /// Save raw YUV frames
    #[arg(long)]
    yuv: bool,
    /// Save temperature planes when present
    #[arg(long)]
    plane: bool,
}

#[derive(clap::Args, Debug)]
struct CaptureArgs {
    /// Output directory
    #[arg(long, default_value = "capture")]
    dir: PathBuf,
    /// File name prefix
    #[arg(long, default_value = "capture")]
    name: String,
    /// PhotoCap timeout (defaults to 30 s)
    #[arg(long = "capture-timeout-ms")]
    capture_timeout_ms: Option<u64>,
    /// Also save the raw framed PhotoCap reply (replay/offline input)
    #[arg(long)]
    raw: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct MeasureArgs {
    /// Display coordinate in the 240x320 frame, e.g. 121,161
    #[arg(long, conflicts_with = "plane")]
    display: Option<String>,
    /// Measurement-plane coordinate, e.g. 96,128 (192x256)
    #[arg(long)]
    plane: Option<String>,
    /// Read a raw f32 LE 192x256 plane instead of using the camera
    #[arg(long, conflicts_with_all = ["capture_file", "usb_file"])]
    temp_file: Option<PathBuf>,
    /// Read a stored PhotoCap reply (framed or bare payload)
    #[arg(long, conflicts_with_all = ["temp_file", "usb_file"])]
    capture_file: Option<PathBuf>,
    /// Read a USB radiometric container directly
    #[arg(long, conflicts_with_all = ["temp_file", "capture_file"])]
    usb_file: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct ImportArgs {
    /// Mounted camera root, DCIM directory, or one .jpg file (default: scan mounts)
    #[arg(long)]
    path: Option<PathBuf>,
    /// Host cache and index directory
    #[arg(long, default_value = "usb-cache")]
    cache: PathBuf,
    /// Only list already imported entries
    #[arg(long)]
    list: bool,
    /// `AUTORUN.INF` marker used to recognise the camera volume
    #[arg(long)]
    usb_marker: Option<String>,
}

#[derive(clap::Args, Debug)]
struct PaletteArgs {
    /// Verified palette index 0..=7
    #[arg(value_parser = parse_palette_index)]
    index: u8,
    /// Frames to inspect after the request
    #[arg(long, default_value_t = 6)]
    frames: u32,
    /// Return success even when the frame signature does not match
    #[arg(long)]
    allow_mismatch: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum UnitArg {
    Celsius,
    Kelvin,
    Flag2Clear,
    Flag2Set,
}

impl UnitArg {
    fn command(self) -> UnitCommand {
        match self {
            Self::Celsius => UnitCommand::Celsius,
            Self::Kelvin => UnitCommand::Kelvin,
            Self::Flag2Clear => UnitCommand::Flag2Clear,
            Self::Flag2Set => UnitCommand::Flag2Set,
        }
    }

    fn label(self) -> &'static str {
        self.command().label()
    }
}

#[derive(clap::Args, Debug)]
struct UnitArgs {
    /// Verified unit command
    #[arg(value_enum)]
    unit: UnitArg,
    /// Wait before re-reading init so the device can apply the change
    #[arg(long, default_value_t = 800)]
    settle_ms: u64,
    /// Return success even when init does not report the requested unit
    #[arg(long)]
    allow_mismatch: bool,
}

#[derive(clap::Args, Debug)]
struct WatchArgs {
    /// Stop after N frames (default: run until interrupted)
    #[arg(long)]
    frames: Option<u64>,
    /// Stop after this many milliseconds
    #[arg(long)]
    duration_ms: Option<u64>,
    /// Also sample this display coordinate on every plane frame
    #[arg(long)]
    sample: Option<String>,
}

/// Parse and run the CLI. `Err` is printed by the bin wrapper.
pub fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match run_inner(&cli) {
        Ok(0) => Ok(()),
        Ok(code) => Err(format!("command failed (exit {code})")),
        Err(err) => Err(err),
    }
}

fn run_inner(cli: &Cli) -> Result<i32, String> {
    match &cli.command {
        CommandLine::Info => cmd_info(cli),
        CommandLine::Frame(args) => cmd_frame(cli, args),
        CommandLine::Frames(args) => cmd_frames(cli, args),
        CommandLine::Capture(args) => cmd_capture(cli, args),
        CommandLine::Measure(args) => cmd_measure(cli, args),
        CommandLine::Palette(args) => cmd_palette(cli, args),
        CommandLine::Unit(args) => cmd_unit(cli, args),
        CommandLine::Shutter => cmd_shutter(cli),
        CommandLine::Watch(args) => cmd_watch(cli, args),
        CommandLine::Import(args) => cmd_import(cli, args),
    }
}

// ---------------------------------------------------------------------------
// Camera session (worker-backed)
// ---------------------------------------------------------------------------

struct Camera {
    worker: Worker,
    init: Option<InitReply>,
    state: ConnState,
    init_seq: u64,
    capture_seq: u64,
    capture_rejected: Option<String>,
    capture_unavailable: Option<String>,
    last_capture: Option<Capture>,
    last_seq: u64,
    stats: NetStats,
    last_error: Option<String>,
    timeout: Duration,
    quiet: bool,
}

impl Camera {
    fn open(cli: &Cli) -> Result<Self, String> {
        let (mut config, warnings) = Config::load(cli.config.as_deref())?;
        for warning in warnings {
            eprintln!("config: {warning}");
        }
        if let Some(ip) = &cli.ip {
            config.camera = if ip.eq_ignore_ascii_case("auto") {
                CameraAddr::Auto
            } else {
                CameraAddr::Manual(
                    ip.parse::<Ipv4Addr>()
                        .map_err(|_| format!("--ip: {ip:?} is not an IPv4 address"))?,
                )
            };
        }
        if let Some(port) = cli.port {
            if port == 0 {
                return Err("--port must be 1..=65535".to_owned());
            }
            config.live_port = port;
        }
        if let Some(fps) = cli.fps {
            if !(1..=60).contains(&fps) {
                return Err(format!("--fps {fps} is outside 1..=60"));
            }
            config.target_fps = fps;
        }
        if let Some(ms) = cli.connect_timeout_ms {
            config.connect_timeout_ms = ms.max(1);
        }
        if let Some(ms) = cli.read_timeout_ms {
            config.read_timeout_ms = ms.max(1);
        }

        let ip = config.resolve_ip()?;
        let addr = SocketAddr::from((ip, config.live_port));
        let timeout = Duration::from_millis(cli.timeout_ms.max(100));

        let worker =
            Worker::spawn().map_err(|err| format!("cannot start network worker: {err}"))?;
        worker.start(StreamConfig {
            addr,
            connect_timeout: config.connect_timeout(),
            read_timeout: config.read_timeout(),
            target_fps: config.target_fps,
        });

        let mut camera = Self {
            worker,
            init: None,
            state: ConnState::Idle,
            init_seq: 0,
            capture_seq: 0,
            capture_rejected: None,
            capture_unavailable: None,
            last_capture: None,
            last_seq: 0,
            stats: NetStats::default(),
            last_error: None,
            timeout,
            quiet: cli.quiet,
        };
        camera.wait_streaming(timeout)?;
        camera.last_seq = camera
            .worker
            .latest_frame()
            .map(|frame| frame.seq)
            .unwrap_or(0);
        Ok(camera)
    }

    fn pump(&mut self) {
        for event in self.worker.try_events() {
            match event {
                Event::Init(reply) => {
                    self.init = Some(*reply);
                    self.init_seq = self.init_seq.saturating_add(1);
                }
                Event::State { state, detail } => {
                    if matches!(state, ConnState::Reconnecting | ConnState::Stopped) {
                        self.last_error = Some(detail.clone());
                        if !self.quiet {
                            eprintln!("camera: {}: {detail}", state.label());
                        }
                    }
                    self.state = state;
                }
                Event::Log(line) => {
                    if line.level != Level::Info {
                        self.last_error = Some(line.text.clone());
                        if !self.quiet {
                            eprintln!("camera: {}", line.text);
                        }
                    }
                }
                Event::Stats(stats) => self.stats = stats,
                Event::Capture(capture) => {
                    self.last_capture = Some(*capture);
                    self.capture_seq = self.capture_seq.saturating_add(1);
                }
                Event::CaptureRejected(detail) => {
                    self.capture_rejected = Some(detail);
                }
                Event::CaptureUnavailable(detail) => {
                    self.capture_unavailable = Some(detail);
                }
            }
        }
    }

    fn wait_streaming(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.state == ConnState::Streaming && self.init.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "camera did not reach streaming within {} ms{}",
                    timeout.as_millis(),
                    self.error_suffix()
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn error_suffix(&self) -> String {
        match &self.last_error {
            Some(err) => format!(": {err}"),
            None => String::new(),
        }
    }

    fn ensure_streaming(&mut self) -> Result<(), String> {
        self.pump();
        if self.state != ConnState::Streaming {
            self.wait_streaming(self.timeout)?;
        }
        Ok(())
    }

    fn send(&mut self, command: Command) -> Result<(), String> {
        self.ensure_streaming()?;
        self.worker.run(command);
        Ok(())
    }

    /// Wait for the next frame newer than the last returned one.
    fn next_frame(&mut self) -> Result<Arc<LiveFrame>, String> {
        let deadline = Instant::now() + self.timeout;
        loop {
            self.pump();
            let latest = self.worker.latest_frame();
            if let Some(frame) = latest
                && frame.seq > self.last_seq
            {
                self.last_seq = frame.seq;
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no frame within {} ms{}",
                    self.timeout.as_millis(),
                    self.error_suffix()
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_init_after(&mut self, mark: u64, timeout: Duration) -> Result<InitReply, String> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.init_seq > mark
                && let Some(init) = self.init.clone()
            {
                return Ok(init);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no init reply within {} ms{}",
                    timeout.as_millis(),
                    self.error_suffix()
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn run_capture(&mut self, timeout: Duration) -> Result<Capture, String> {
        self.ensure_streaming()?;
        let mark = self.capture_seq;
        self.capture_rejected = None;
        self.capture_unavailable = None;
        self.worker.run(Command::PhotoCap);
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if let Some(reason) = self.capture_unavailable.take() {
                return Err(format!("capture unavailable: {reason}"));
            }
            if let Some(reason) = self.capture_rejected.take() {
                return Err(format!("retryable: {reason}"));
            }
            if self.capture_seq > mark
                && let Some(capture) = self.last_capture.take()
            {
                return Ok(capture);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "PhotoCap did not complete within {} ms{}",
                    timeout.as_millis(),
                    self.error_suffix()
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

fn cmd_info(cli: &Cli) -> Result<i32, String> {
    let mut camera = Camera::open(cli)?;
    camera.pump();
    let init = camera
        .init
        .clone()
        .ok_or_else(|| "camera did not return init".to_owned())?;
    emit(cli.json, info_json(&init), || print_info(&init));
    Ok(0)
}

fn cmd_frame(cli: &Cli, args: &FrameArgs) -> Result<i32, String> {
    let mut camera = Camera::open(cli)?;
    let mut frame = None;
    for _ in 0..args.max_frames.max(1) {
        let candidate = camera.next_frame()?;
        let has_plane = candidate.plane_c.is_some();
        frame = Some(candidate);
        if !args.require_plane || has_plane {
            break;
        }
    }
    let frame = frame.ok_or_else(|| "no frame received".to_owned())?;

    let png = save_optional(args.png.as_deref(), |path| {
        write_png(path, &frame.rgba, frame.display_w, frame.display_h)
    })?;
    let yuv = save_optional(args.yuv.as_deref(), |path| {
        fs::write(path, &frame.yuv).map_err(|err| format!("cannot write {}: {err}", path.display()))
    })?;
    let plane = match (&args.plane, &frame.plane_c) {
        (Some(path), Some(plane)) => Some(save_plane(path, plane)?),
        (Some(_), None) => None,
        (None, _) => None,
    };

    let value = frame_json(&frame, png.as_deref(), yuv.as_deref(), plane.as_deref());
    emit(cli.json, value.clone(), || print_frame(&value));
    if args.require_plane && frame.plane_c.is_none() {
        return Err("extended frame with a live plane was not received".to_owned());
    }
    Ok(0)
}

fn cmd_frames(cli: &Cli, args: &FramesArgs) -> Result<i32, String> {
    fs::create_dir_all(&args.dir)
        .map_err(|err| format!("cannot create {}: {err}", args.dir.display()))?;
    let save_any = args.png || args.yuv || args.plane;
    let save_png = args.png || !save_any;
    let save_yuv = args.yuv || !save_any;

    let mut camera = Camera::open(cli)?;
    let mut frames = Vec::with_capacity(args.count as usize);
    for _ in 0..args.count.max(1) {
        let frame = camera.next_frame()?;
        let stem = format!("frame_{:06}", frame.seq);
        let png = if save_png {
            let path = args.dir.join(format!("{stem}.png"));
            write_png(&path, &frame.rgba, frame.display_w, frame.display_h)?;
            Some(path)
        } else {
            None
        };
        let yuv = if save_yuv {
            let extension = match frame.format {
                crate::model::PixelFormat::I420 => "yuv",
                crate::model::PixelFormat::Rgb888 => "rgb",
            };
            let path = args.dir.join(format!("{stem}.{extension}"));
            fs::write(&path, &frame.yuv)
                .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
            Some(path)
        } else {
            None
        };
        let plane = if args.plane {
            if let Some(plane) = &frame.plane_c {
                let path = args.dir.join(format!("{stem}_temp.f32"));
                Some(save_plane(&path, plane)?)
            } else {
                None
            }
        } else {
            None
        };
        frames.push(frame_json(
            &frame,
            png.as_deref(),
            yuv.as_deref(),
            plane.as_deref(),
        ));
    }

    let value = json!({ "count": frames.len(), "dir": args.dir, "frames": frames });
    emit(cli.json, value.clone(), || print_frames(&value));
    Ok(0)
}

fn cmd_capture(cli: &Cli, args: &CaptureArgs) -> Result<i32, String> {
    fs::create_dir_all(&args.dir)
        .map_err(|err| format!("cannot create {}: {err}", args.dir.display()))?;
    let mut camera = Camera::open(cli)?;
    let timeout = args
        .capture_timeout_ms
        .map(|ms| Duration::from_millis(ms.max(100)))
        .unwrap_or_else(|| Duration::from_millis(CAPTURE_TIMEOUT_MS));
    let capture = camera.run_capture(timeout)?;

    let prefix = &args.name;
    let shot = args.dir.join(format!("{prefix}_shot.jpg"));
    let vis = args.dir.join(format!("{prefix}_vis.jpg"));
    let temp = args.dir.join(format!("{prefix}_temperature.f32"));
    let y16 = args.dir.join(format!("{prefix}_y16.u16"));
    let tail = args.dir.join(format!("{prefix}_tail.bin"));
    let raw = args.raw.clone();

    fs::write(&shot, &capture.shot_jpeg)
        .map_err(|err| format!("cannot write {}: {err}", shot.display()))?;
    fs::write(&vis, &capture.vis_jpeg)
        .map_err(|err| format!("cannot write {}: {err}", vis.display()))?;
    capture
        .write_temperature_plane(&temp)
        .map_err(|err| format!("cannot write {}: {err}", temp.display()))?;
    capture
        .write_y16_plane(&y16)
        .map_err(|err| format!("cannot write {}: {err}", y16.display()))?;
    fs::write(&tail, &capture.tail)
        .map_err(|err| format!("cannot write {}: {err}", tail.display()))?;
    if let Some(path) = &raw {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        fs::write(path, capture.to_message())
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    }

    let stats = capture.stats();
    let value = json!({
        "sections": {
            "shot_jpeg": {"bytes": capture.shot_jpeg.len(), "path": shot},
            "temperature_c": {"bytes": capture.temperature_c.len() * 4, "samples": capture.temperature_c.len(), "width": capture.raw_w, "height": capture.raw_h, "path": temp},
            "vis_jpeg": {"bytes": capture.vis_jpeg.len(), "path": vis},
            "y16": {"bytes": capture.y16.len(), "path": y16},
            "tail": {"bytes": capture.tail.len(), "path": tail},
        },
        "raw_message": raw,
        "temperature": {
            "min_c": stats.min,
            "max_c": stats.max,
            "mean_c": stats.mean,
            "std_dev_c": stats.std_dev,
        },
        "geometry": {
            "display": [capture.display_w, capture.display_h],
            "raw": [capture.raw_w, capture.raw_h],
        },
        "mapping": format!(
            "display x {:.4}/{:.4}, no rotation or flip",
            capture.raw_w as f32 / capture.display_w as f32,
            capture.raw_h as f32 / capture.display_h as f32
        ),
    });
    emit(cli.json, value.clone(), || print_capture(&value));
    Ok(0)
}

fn cmd_measure(cli: &Cli, args: &MeasureArgs) -> Result<i32, String> {
    let pair = args
        .display
        .as_deref()
        .map(|value| parse_pair(value, "display"))
        .transpose()?;
    let plane_pair = args
        .plane
        .as_deref()
        .map(|value| parse_pair(value, "plane"))
        .transpose()?;

    let (source, plane, raw_w, raw_h, display_w, display_h) = if let Some(path) = &args.temp_file {
        let plane = read_plane_file(path)?;
        (
            format!("file:{}", path.display()),
            plane,
            capture::TEMP_PLANE_W,
            capture::TEMP_PLANE_H,
            protocol::FRAME_W,
            protocol::FRAME_H,
        )
    } else if let Some(path) = &args.capture_file {
        let capture = capture::load_capture_file(path)?;
        (
            format!("capture:{}", path.display()),
            capture.temperature_c,
            capture.raw_w,
            capture.raw_h,
            capture.display_w,
            capture.display_h,
        )
    } else if let Some(path) = &args.usb_file {
        let image = usb::parse_usb_file(path)?;
        (
            format!("usb:{}", path.display()),
            image.temperature_c,
            capture::TEMP_PLANE_W,
            capture::TEMP_PLANE_H,
            protocol::FRAME_W,
            protocol::FRAME_H,
        )
    } else {
        let mut camera = Camera::open(cli)?;
        let deadline = Instant::now() + camera.timeout;
        let mut found = None;
        while found.is_none() {
            let frame = camera.next_frame()?;
            if let Some(live) = frame.plane_c.clone() {
                found = Some((frame, live));
            }
            if Instant::now() >= deadline && found.is_none() {
                return Err(format!(
                    "no extended frame with a live plane within {} ms{}",
                    camera.timeout.as_millis(),
                    camera.error_suffix()
                ));
            }
        }
        let (frame, plane) = found.expect("just checked");
        (
            "live".to_owned(),
            plane,
            frame.raw_w,
            frame.raw_h,
            frame.display_w,
            frame.display_h,
        )
    };

    let (display_x, display_y, plane_x, plane_y) = match (pair, plane_pair) {
        (Some((x, y)), None) => {
            let (px, py) = capture::plane_coords_for(x, y, display_w, display_h, raw_w, raw_h)
                .ok_or_else(|| {
                    format!("display coordinate ({x}, {y}) is outside {display_w}x{display_h}")
                })?;
            (Some(x), Some(y), px, py)
        }
        (None, Some((px, py))) => {
            let px = px.floor() as isize;
            let py = py.floor() as isize;
            if !(0..raw_w as isize).contains(&px) || !(0..raw_h as isize).contains(&py) {
                return Err(format!(
                    "plane coordinate ({px}, {py}) is outside {raw_w}x{raw_h}"
                ));
            }
            (None, None, px as usize, py as usize)
        }
        (None, None) => (None, None, raw_w / 2, raw_h / 2),
        (Some(_), Some(_)) => return Err("use either --display or --plane".to_owned()),
    };

    let temperature_c = plane
        .get(plane_y * raw_w + plane_x)
        .copied()
        .ok_or_else(|| "plane sample out of range".to_owned())?;
    let value = json!({
        "temperature_c": temperature_c,
        "source": source,
        "display": display_x.zip(display_y).map(|(x, y)| json!([x, y])),
        "display_size": [display_w, display_h],
        "raw_size": [raw_w, raw_h],
        "plane": [plane_x, plane_y],
        "mapping": format!(
            "display x {:.4}/{:.4}, no rotation or flip",
            raw_w as f32 / display_w as f32,
            raw_h as f32 / display_h as f32
        ),
    });
    emit(cli.json, value.clone(), || print_measure(&value));
    Ok(0)
}

fn cmd_palette(cli: &Cli, args: &PaletteArgs) -> Result<i32, String> {
    let requested =
        Palette::from_index(args.index).ok_or_else(|| "palette index must be 0..=7".to_owned())?;
    let mut camera = Camera::open(cli)?;
    camera.send(Command::ColorChange(requested))?;

    let mut samples = Vec::new();
    let mut confirmed = false;
    let mut best = (requested, f32::INFINITY);
    let mut detected_ok = None;
    for _ in 0..args.frames.max(1) {
        let frame = camera.next_frame()?;
        let signature = frame_signature(&frame.rgba);
        let (detected, distance) = nearest_palette(signature);
        samples.push(json!({
            "seq": frame.seq,
            "detected": detected.index(),
            "distance": distance,
        }));
        if distance < best.1 {
            best = (detected, distance);
        }
        if distance <= PALETTE_MATCH_DISTANCE && equivalent(requested, detected) {
            confirmed = true;
            detected_ok = Some((detected, distance));
            break;
        }
    }
    let (detected, distance) = detected_ok.unwrap_or(best);
    let value = json!({
        "requested": requested.index(),
        "detected": detected.index(),
        "distance": distance,
        "confirmed": confirmed,
        "samples": samples,
        "note": "signature heuristic against spec §3.1 reference means",
    });
    emit(cli.json, value.clone(), || print_palette(&value));
    if !confirmed && !args.allow_mismatch {
        return Err(format!(
            "palette {} not confirmed (frame resembles {})",
            requested.index(),
            detected.index()
        ));
    }
    Ok(0)
}

fn cmd_unit(cli: &Cli, args: &UnitArgs) -> Result<i32, String> {
    let mut camera = Camera::open(cli)?;
    let command = args.unit.command();
    camera.send(Command::MeasureUnit(command))?;
    std::thread::sleep(Duration::from_millis(args.settle_ms));
    let mark = camera.init_seq;
    camera.send(Command::Init)?;
    let init = camera.wait_init_after(mark, camera.timeout)?;

    let confirmed = command.matches_byte(init.unit);
    let value = json!({
        "requested": args.unit.label(),
        "reported_unit_byte": init.unit,
        "reported_unit": init.unit_label(),
        "kelvin": init.is_kelvin(),
        "secondary_flag": init.secondary_flag(),
        "confirmed": confirmed,
    });
    emit(cli.json, value.clone(), || print_unit(&value));
    if !confirmed && !args.allow_mismatch {
        return Err(format!(
            "device reports unit byte {} ({}) after requesting {}",
            init.unit,
            init.unit_label(),
            args.unit.label()
        ));
    }
    Ok(0)
}

fn cmd_shutter(cli: &Cli) -> Result<i32, String> {
    let mut camera = Camera::open(cli)?;
    camera.send(Command::Shutter)?;
    let value = json!({
        "command": Command::Shutter.wire(),
        "reply": "none",
        "note": "flat-field correction requested; measurement baseline resets when the temperature step is detected",
    });
    emit(cli.json, value.clone(), || {
        println!("FFC requested: {}", Command::Shutter.wire());
        println!("No reply is expected; watch the key temperatures for the FFC step.");
    });
    Ok(0)
}

fn cmd_watch(cli: &Cli, args: &WatchArgs) -> Result<i32, String> {
    let mut camera = Camera::open(cli)?;
    let sample_coords = args
        .sample
        .as_deref()
        .map(|value| parse_pair(value, "sample"))
        .transpose()?;
    let deadline = args
        .duration_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut last_plane: Option<(Vec<f32>, usize, usize, usize, usize)> = None;
    let mut emitted = 0u64;

    loop {
        if args.frames.is_some_and(|limit| emitted >= limit) {
            break;
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            break;
        }
        let frame = camera.next_frame()?;
        if let Some(plane) = &frame.plane_c {
            last_plane = Some((
                plane.clone(),
                frame.raw_w,
                frame.raw_h,
                frame.display_w,
                frame.display_h,
            ));
        }
        let sample = sample_coords.and_then(|(x, y)| {
            last_plane
                .as_ref()
                .and_then(|(plane, raw_w, raw_h, display_w, display_h)| {
                    capture::sample_plane_for(plane, x, y, *display_w, *display_h, *raw_w, *raw_h)
                })
        });
        let plane_stats = last_plane
            .as_ref()
            .and_then(|(plane, _, _, _, _)| capture::plane_stats(plane));

        if cli.json {
            let value = json!({
                "seq": frame.seq,
                "rtt_ms": frame.rtt.as_secs_f32() * 1000.0,
                "key_c": frame.key_c,
                "has_plane": frame.plane_c.is_some(),
                "plane": plane_stats.map(|stats| json!({
                    "min_c": stats.min,
                    "max_c": stats.max,
                    "mean_c": stats.mean,
                })),
                "sample_c": sample,
            });
            println!("{value}");
        } else {
            let keys = frame
                .key_c
                .iter()
                .map(|value| format!("{value:.2}"))
                .collect::<Vec<_>>()
                .join("/");
            let sample_text = sample
                .map(|value| format!(" sample={value:.2}"))
                .unwrap_or_default();
            let plane_text = if frame.plane_c.is_some() {
                " plane=yes"
            } else {
                ""
            };
            println!("seq={}{plane_text}{sample_text} keys={keys}", frame.seq);
        }
        emitted = emitted.saturating_add(1);
    }
    Ok(0)
}

fn cmd_import(cli: &Cli, args: &ImportArgs) -> Result<i32, String> {
    let (config, warnings) = Config::load(cli.config.as_deref())?;
    for warning in warnings {
        eprintln!("config: {warning}");
    }
    let marker = args.usb_marker.as_deref().unwrap_or(&config.usb_marker);
    let mut index = usb::load_index(&args.cache);
    if args.list {
        usb::refresh_index(&args.cache, &mut index);
        let entries: Vec<Value> = index.entries.values().map(usb_entry_json).collect();
        let value = json!({ "cache": args.cache, "entries": entries });
        emit(cli.json, value.clone(), || print_usb_index(&value));
        return Ok(0);
    }

    let volumes = usb::discover_volumes_with_marker(args.path.as_deref(), marker);
    if volumes.is_empty() {
        return Err(
            "no UTi260B+ volume found. Mount the camera read-only and pass --path, \
             or point --path at a DCIM directory or .jpg file; use --usb-marker for \
             a different AUTORUN.INF label."
                .to_owned(),
        );
    }

    let mut imported = Vec::new();
    let mut duplicates = 0usize;
    let mut errors = Vec::new();
    for volume in &volumes {
        for file in &volume.files {
            match usb::import_file(file, &args.cache, &mut index) {
                Ok(outcome) if outcome.duplicate => duplicates += 1,
                Ok(outcome) => {
                    let mut value = usb_entry_json(&outcome.entry);
                    value["duplicate"] = json!(false);
                    imported.push(value);
                }
                Err(err) => errors.push(format!("{}: {err}", file.display())),
            }
        }
    }

    let value = json!({
        "cache": args.cache,
        "volumes": volumes.len(),
        "imported": imported,
        "duplicates": duplicates,
        "errors": errors,
    });
    emit(cli.json, value.clone(), || print_usb_import(&value));
    if !value["errors"].as_array().is_some_and(Vec::is_empty) {
        return Err(format!(
            "{} USB file(s) unreadable; see errors",
            value["errors"].as_array().map(Vec::len).unwrap_or(0)
        ));
    }
    Ok(0)
}

fn usb_entry_json(entry: &IndexEntry) -> Value {
    json!({
        "sha256": entry.sha256,
        "file": entry.source_name,
        "captured": entry.captured,
        "cache_file": entry.cache_file,
        "thumbnail_file": entry.thumbnail_file,
        "metadata": entry.metadata,
        "temperature": {
            "min_c": entry.min_c,
            "max_c": entry.max_c,
            "mean_c": entry.mean_c,
            "std_dev_c": entry.std_dev_c,
        },
        "imported_at": entry.imported_at,
    })
}

fn print_usb_index(value: &Value) {
    println!("USB cache: {}", value["cache"]);
    if let Some(entries) = value["entries"].as_array() {
        for entry in entries {
            println!(
                "{}  {}  {:.2}..{:.2} °C  fw {}",
                entry["sha256"]
                    .as_str()
                    .unwrap_or("")
                    .get(..12)
                    .unwrap_or(""),
                entry["file"].as_str().unwrap_or(""),
                entry["temperature"]["min_c"].as_f64().unwrap_or(0.0),
                entry["temperature"]["max_c"].as_f64().unwrap_or(0.0),
                entry["metadata"]["firmware"].as_str().unwrap_or("?"),
            );
        }
    }
}

fn print_usb_import(value: &Value) {
    let imported = value["imported"].as_array().map(Vec::len).unwrap_or(0);
    let duplicates = value["duplicates"].as_u64().unwrap_or(0);
    println!(
        "USB import: {imported} new, {duplicates} duplicate(s), {} error(s)",
        value["errors"].as_array().map(Vec::len).unwrap_or(0)
    );
    if let Some(entries) = value["imported"].as_array() {
        for entry in entries {
            println!(
                "  {}  {:.2}..{:.2} °C  {}",
                entry["file"].as_str().unwrap_or(""),
                entry["temperature"]["min_c"].as_f64().unwrap_or(0.0),
                entry["temperature"]["max_c"].as_f64().unwrap_or(0.0),
                entry["sha256"]
                    .as_str()
                    .unwrap_or("")
                    .get(..12)
                    .unwrap_or(""),
            );
        }
    }
    if let Some(errors) = value["errors"].as_array() {
        for error in errors {
            eprintln!("USB error: {}", error.as_str().unwrap_or(""));
        }
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn emit(json_mode: bool, value: Value, human: impl FnOnce()) {
    if json_mode {
        println!("{value}");
    } else {
        human();
    }
}

fn print_info(init: &InitReply) {
    println!(
        "versions: {} / {} / {}",
        init.versions[0], init.versions[1], init.versions[2]
    );
    println!("emissivity: {:.2}", init.emissivity);
    println!("distance_m: {:.2}", init.distance_m);
    println!("ambient: {:.2} ({})", init.ambient_c, init.unit_label());
    println!("humidity_pct: {}", init.humidity_pct);
    println!("unit: {} (byte {})", init.unit_label(), init.unit);
    let scale = init.color_scale_fields();
    println!(
        "colour scale: palette +{}, mode {}, auto {:.2}…{:.2} °C, manual {:.2}…{:.2} °C",
        scale.palette,
        scale.mode,
        scale.auto_lower_c,
        scale.auto_upper_c,
        scale.manual_lower_c,
        scale.manual_upper_c
    );
    println!("image_select: {}", init.image_select);
    println!("fusion_level: {:.2}", init.fusion_level);
}

fn print_frame(value: &Value) {
    println!(
        "seq={} rtt_ms={:.1} keys_c={} plane={}",
        value["seq"],
        value["rtt_ms"].as_f64().unwrap_or(0.0),
        value["key_c"],
        value["plane"]["present"].as_bool().unwrap_or(false),
    );
}

fn print_frames(value: &Value) {
    println!("saved {} frames", value["count"]);
    if let Some(frames) = value["frames"].as_array() {
        for frame in frames {
            print_frame(frame);
        }
    }
}

fn print_capture(value: &Value) {
    let stats = &value["temperature"];
    println!(
        "capture: temp {:.2}..{:.2} °C mean {:.2} ({} sections)",
        stats["min_c"].as_f64().unwrap_or(0.0),
        stats["max_c"].as_f64().unwrap_or(0.0),
        stats["mean_c"].as_f64().unwrap_or(0.0),
        value["sections"]
            .as_object()
            .map(|map| map.len())
            .unwrap_or(0),
    );
    if let Some(sections) = value["sections"].as_object() {
        for (name, section) in sections {
            if let Some(path) = section["path"].as_str() {
                println!("  {name}: {path}");
            }
        }
    }
}

fn print_measure(value: &Value) {
    println!(
        "{} °C at plane {} (source {})",
        value["temperature_c"], value["plane"], value["source"]
    );
}

fn print_palette(value: &Value) {
    println!(
        "palette requested {} detected {} distance {:.1} confirmed={}",
        value["requested"],
        value["detected"],
        value["distance"].as_f64().unwrap_or(0.0),
        value["confirmed"].as_bool().unwrap_or(false)
    );
}

fn print_unit(value: &Value) {
    println!(
        "unit requested {} reported {} (byte {}) confirmed={}",
        value["requested"],
        value["reported_unit"],
        value["reported_unit_byte"],
        value["confirmed"].as_bool().unwrap_or(false)
    );
}

fn info_json(init: &InitReply) -> Value {
    json!({
        "versions": init.versions,
        "emissivity": init.emissivity,
        "distance_m": init.distance_m,
        "ambient": init.ambient_c,
        "ambient2": init.ambient2_c,
        "humidity_pct": init.humidity_pct,
        "ambient_int": init.ambient_int_c,
        "unit": {
            "byte": init.unit,
            "label": init.unit_label(),
            "kelvin": init.is_kelvin(),
            "secondary_flag": init.secondary_flag(),
        },
        "image_select": init.image_select,
        "fusion_level": init.fusion_level,
        "pip_rect": init.pip_rect,
        "mark_flags": init.mark_flags,
        "alarm": {
            "enabled": init.alarm_enable != 0,
            "high_delta_k": init.alarm_high_delta_k,
            "value": init.alarm_value_c,
        },
        "camera_info": init.camera_info,
        "other_info": init.other_info,
        "laser_px": init.laser_px,
        "fuse_xy": init.fuse_xy,
        "colour_scale": {
            "palette": init.color_scale_fields().palette,
            "mode": init.color_scale_fields().mode,
            "auto_upper_c": init.color_scale_fields().auto_upper_c,
            "auto_lower_c": init.color_scale_fields().auto_lower_c,
            "manual_upper_c": init.color_scale_fields().manual_upper_c,
            "manual_lower_c": init.color_scale_fields().manual_lower_c,
        },
        "colour_scale_raw": hex_string(&init.color_scale),
    })
}

fn frame_json(
    frame: &LiveFrame,
    png: Option<&Path>,
    yuv: Option<&Path>,
    plane_path: Option<&Path>,
) -> Value {
    let plane_stats = frame.plane_c.as_deref().and_then(capture::plane_stats);
    json!({
        "seq": frame.seq,
        "rtt_ms": frame.rtt.as_secs_f32() * 1000.0,
        "geometry": {
            "display": [frame.display_w, frame.display_h],
            "raw": [frame.raw_w, frame.raw_h],
            "format": frame.format.to_string(),
        },
        "key_c": frame.key_c,
        "plane": {
            "present": frame.plane_c.is_some(),
            "path": plane_path,
            "stats": plane_stats.map(|stats| json!({
                "min_c": stats.min,
                "max_c": stats.max,
                "mean_c": stats.mean,
                "std_dev_c": stats.std_dev,
            })),
        },
        "files": {"png": png, "yuv": yuv},
    })
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

fn save_optional(
    path: Option<&Path>,
    save: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Option<PathBuf>, String> {
    match path {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
            }
            save(path)?;
            Ok(Some(path.to_path_buf()))
        }
        None => Ok(None),
    }
}

fn save_plane(path: &Path, plane: &[f32]) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let mut bytes = Vec::with_capacity(plane.len() * 4);
    for value in plane {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(path.to_path_buf())
}

fn write_png(path: &Path, rgba: &[u8], width: usize, height: usize) -> Result<(), String> {
    let file =
        fs::File::create(path).map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| format!("writing {}: {err}", path.display()))?;
    writer
        .write_image_data(rgba)
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

fn read_plane_file(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    if bytes.len() != capture::TEMP_PLANE_BYTES {
        return Err(format!(
            "{}: plane is {} bytes, expected {}",
            path.display(),
            bytes.len(),
            capture::TEMP_PLANE_BYTES
        ));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect())
}

fn hex_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn parse_pair(value: &str, what: &str) -> Result<(f32, f32), String> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| format!("--{what} expects X,Y (got {value:?})"))?;
    let x = x
        .trim()
        .parse::<f32>()
        .map_err(|_| format!("--{what}: {x:?} is not a number"))?;
    let y = y
        .trim()
        .parse::<f32>()
        .map_err(|_| format!("--{what}: {y:?} is not a number"))?;
    Ok((x, y))
}

fn parse_palette_index(value: &str) -> Result<u8, String> {
    let index = value
        .parse::<u8>()
        .map_err(|_| format!("palette index {value:?} is not 0..=7"))?;
    if index > 7 {
        return Err(format!(
            "palette index {index} is outside the verified 0..=7 range"
        ));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_parser() {
        assert_eq!(parse_pair("121,161", "display").unwrap(), (121.0, 161.0));
        assert!(parse_pair("121", "display").is_err());
        assert!(parse_pair("a,b", "display").is_err());
    }

    #[test]
    fn palette_index_range() {
        assert_eq!(parse_palette_index("0").unwrap(), 0);
        assert_eq!(parse_palette_index("7").unwrap(), 7);
        assert!(parse_palette_index("8").is_err());
    }
}
