//! egui front end.
//!
//! The UI thread only ever touches the newest decoded frame (via `ArcSwap`) and
//! a small event stream from the worker; all socket I/O and YUV conversion stay
//! off the paint path (§8.1, §8.2).
//!
//! Commands are a typed whitelist (§3, §13.10): the device daemon exits on a
//! malformed command, so there is no free-form command input.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capture::{self, Capture, TempStats};
use crate::config::Config;
use crate::frame::LiveFrame;
use crate::net::{ConnState, Event, Level, NetStats, StreamConfig, Worker};
use crate::palette::{PALETTE_MATCH_DISTANCE, equivalent, frame_signature, nearest_palette};
use crate::protocol::{self, Command, FRAME_H, FRAME_W, InitReply, Palette, UnitCommand};
use crate::replay::ReplayWorker;
use crate::usb::UsbMetadata;
use crate::usb_worker::{ImportedUsb, UsbEvent, UsbWorker};

const LOG_CAPACITY: usize = 400;
const TOAST_SECS: f32 = 6.0;
const NO_FRAME: &str = "—";
/// Where frames come from. Chosen at startup; replay mode never opens a socket.
#[derive(Clone, Debug)]
pub enum Mode {
    Live,
    Replay { path: PathBuf },
}

enum Controller {
    Net(Worker),
    Replay(ReplayWorker),
}

impl Controller {
    fn latest_frame(&self) -> Option<Arc<LiveFrame>> {
        match self {
            Self::Net(worker) => worker.latest_frame(),
            Self::Replay(worker) => worker.latest_frame(),
        }
    }

    fn try_events(&self) -> Vec<Event> {
        match self {
            Self::Net(worker) => worker.try_events(),
            Self::Replay(worker) => worker.try_events(),
        }
    }

    fn start(&self, config: StreamConfig) {
        match self {
            Self::Net(worker) => worker.start(config),
            Self::Replay(worker) => worker.set_running(true),
        }
    }

    fn stop(&self) {
        match self {
            Self::Net(worker) => worker.stop(),
            Self::Replay(worker) => worker.set_running(false),
        }
    }

    fn set_fps(&self, fps: u32) {
        match self {
            Self::Net(worker) => worker.set_fps(fps),
            Self::Replay(worker) => worker.set_fps(fps),
        }
    }

    fn run(&self, command: Command) {
        if let Self::Net(worker) = self {
            worker.run(command);
        }
    }
}

struct LogEntry {
    at: f32,
    level: Level,
    text: String,
}

/// One temperature sample taken from the PhotoCap plane.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// Display coordinates in the 240x320 streamed frame.
    display: [f32; 2],
    /// Measurement-plane coordinates (0..192, 0..256).
    plane: [usize; 2],
    temperature_c: f32,
}

#[derive(Clone, Copy)]
enum CaptureExport {
    Temperature,
    Y16,
}

pub struct CameraApp {
    ctrl: Controller,
    mode: Mode,
    config: Config,

    ip_text: String,
    port_text: String,
    target_fps: u32,
    integer_scaling: bool,

    texture: Option<egui::TextureHandle>,
    texture_nearest: bool,
    last_frame: Option<Arc<LiveFrame>>,
    last_keys: Option<[f32; 3]>,
    last_seq: u64,
    dropped: u64,

    state: ConnState,
    state_detail: String,
    init: Option<InitReply>,
    stats: NetStats,

    capture: Option<Arc<Capture>>,
    capture_stats: Option<TempStats>,
    live_plane: Option<Arc<Vec<f32>>>,
    live_plane_stats: Option<TempStats>,

    usb_worker: Option<UsbWorker>,
    usb_entries: Vec<ImportedUsb>,
    usb_textures: HashMap<String, egui::TextureHandle>,
    usb_volumes: Vec<String>,
    usb_status: String,
    usb_selected: Option<String>,
    usb_plane: Option<Arc<Vec<f32>>>,
    usb_stats: Option<TempStats>,
    usb_meta: Option<UsbMetadata>,
    usb_paused: bool,
    capture_pending: bool,
    capture_pending_since: Option<Instant>,
    sample: Option<Sample>,

    palette_requested: Option<Palette>,
    palette_detected: Option<(Palette, f32)>,
    palette_frames_left: u8,
    unit_requested: Option<UnitCommand>,
    unit_pending: bool,

    log: VecDeque<LogEntry>,
    started: Instant,
    toast: Option<(String, Instant, bool)>,
    stream_requested: bool,
}

impl CameraApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        mode: Mode,
        mut startup_notes: Vec<String>,
    ) -> Result<Self, String> {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let controller = match &mode {
            Mode::Live => Controller::Net(
                Worker::spawn().map_err(|e| format!("cannot start network worker: {e}"))?,
            ),
            Mode::Replay { path } => {
                Controller::Replay(ReplayWorker::spawn(path, config.target_fps)?)
            }
        };

        let usb_worker =
            match UsbWorker::spawn(PathBuf::from("usb-cache"), None, config.usb_marker.clone()) {
                Ok(worker) => Some(worker),
                Err(err) => {
                    startup_notes.push(format!("USB import worker unavailable: {err}"));
                    None
                }
            };

        let ip_text = match config.camera {
            crate::config::CameraAddr::Auto => "auto".to_owned(),
            crate::config::CameraAddr::Manual(ip) => ip.to_string(),
        };

        let mut app = Self {
            ctrl: controller,
            mode,
            port_text: config.live_port.to_string(),
            target_fps: config.target_fps,
            integer_scaling: true,
            texture: None,
            texture_nearest: false,
            last_frame: None,
            last_keys: None,
            last_seq: 0,
            dropped: 0,
            state: ConnState::Idle,
            state_detail: String::new(),
            init: None,
            stats: NetStats::default(),
            capture: None,
            capture_stats: None,
            live_plane: None,
            live_plane_stats: None,
            usb_worker,
            usb_entries: Vec::new(),
            usb_textures: HashMap::new(),
            usb_volumes: Vec::new(),
            usb_status: "scanning for a camera volume".to_owned(),
            usb_selected: None,
            usb_plane: None,
            usb_stats: None,
            usb_meta: None,
            usb_paused: false,
            capture_pending: false,
            capture_pending_since: None,
            sample: None,
            palette_requested: None,
            palette_detected: None,
            palette_frames_left: 0,
            unit_requested: None,
            unit_pending: false,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            started: Instant::now(),
            toast: None,
            stream_requested: false,
            ip_text,
            config,
        };

        if let Some(source) = &app.config.source {
            app.push_log(Level::Info, format!("config: {}", source.display()));
        }
        for note in startup_notes {
            app.push_log(Level::Warn, note);
        }

        match &app.mode {
            Mode::Live => app.push_log(
                Level::Info,
                "live mode — press Connect to resolve the camera address and start streaming",
            ),
            Mode::Replay { path } => {
                app.push_log(Level::Info, format!("replay mode: {}", path.display()))
            }
        }

        Ok(app)
    }

    // -- event pump ---------------------------------------------------------

    fn pump_events(&mut self, ctx: &egui::Context) {
        self.pump_usb_events(ctx);
        for event in self.ctrl.try_events() {
            match event {
                Event::State { state, detail } => {
                    if state != self.state || detail != self.state_detail {
                        let level = match state {
                            ConnState::Reconnecting => Level::Warn,
                            _ => Level::Info,
                        };
                        self.push_log(level, format!("{}: {detail}", state.label()));
                    }
                    if matches!(state, ConnState::Reconnecting | ConnState::Stopped)
                        && (self.capture_pending
                            || self.unit_pending
                            || self.palette_frames_left > 0)
                    {
                        self.push_log(
                            Level::Warn,
                            "pending command confirmation cancelled: link state changed",
                        );
                    }
                    if matches!(state, ConnState::Reconnecting | ConnState::Stopped) {
                        self.capture_pending = false;
                        self.capture_pending_since = None;
                        self.unit_pending = false;
                        self.unit_requested = None;
                        self.palette_frames_left = 0;
                    }
                    self.state = state;
                    self.state_detail = detail;
                    if matches!(self.mode, Mode::Live) {
                        let paused = self.state.is_active();
                        if paused != self.usb_paused {
                            self.usb_paused = paused;
                            if let Some(worker) = &self.usb_worker {
                                worker.set_paused(paused);
                            }
                        }
                    }
                }
                Event::Init(reply) => {
                    self.push_log(
                        Level::Info,
                        format!(
                            "device state: {} / {} / {}, ε {:.2}, {:.2} m, {} %RH, {}",
                            reply.versions[0],
                            reply.versions[1],
                            reply.versions[2],
                            reply.emissivity,
                            reply.distance_m,
                            reply.humidity_pct,
                            reply.unit_label()
                        ),
                    );
                    if let Some(requested) = self.unit_requested.take() {
                        let expected = match requested {
                            UnitCommand::Celsius => 0,
                            UnitCommand::Unit4 => 4,
                            UnitCommand::Kelvin => 6,
                        };
                        self.unit_pending = false;
                        if reply.unit == expected {
                            self.push_log(
                                Level::Info,
                                format!(
                                    "unit confirmed after {:?}: {}",
                                    requested,
                                    reply.unit_label()
                                ),
                            );
                        } else {
                            let msg = format!(
                                "unit change failed: requested {:?} (unit {}), device reports {}",
                                requested,
                                expected,
                                reply.unit_label()
                            );
                            self.push_log(Level::Error, msg.clone());
                            self.toast(msg, true);
                        }
                    }
                    self.init = Some(*reply);
                }
                Event::Stats(stats) => self.stats = stats,
                Event::CaptureRejected(detail) => {
                    self.capture_pending = false;
                    self.capture_pending_since = None;
                    self.push_log(Level::Warn, detail.clone());
                    self.toast(detail, true);
                }
                Event::Capture(capture) => {
                    let stats = capture.stats();
                    self.capture_stats = Some(stats);
                    self.capture_pending = false;
                    self.capture_pending_since = None;
                    self.sample = None;
                    self.push_log(
                        Level::Info,
                        format!(
                            "capture ready: 192×256 plane, {:.2}..{:.2} °C, mean {:.2}; \
                             shot {} B, vis {} B",
                            stats.min,
                            stats.max,
                            stats.mean,
                            capture.shot_jpeg.len(),
                            capture.vis_jpeg.len()
                        ),
                    );
                    self.toast("PhotoCap ready — click the image to sample", false);
                    self.capture = Some(Arc::new(*capture));
                }
                Event::Log(line) => self.push_log(line.level, line.text),
            }
        }

        if self.capture_pending
            && self
                .capture_pending_since
                .is_some_and(|since| since.elapsed() > Duration::from_secs(20))
        {
            self.capture_pending = false;
            self.capture_pending_since = None;
            let msg = "PhotoCap timed out or was rejected; see the log";
            self.push_log(Level::Error, msg.to_owned());
            self.toast(msg, true);
        }

        if let Some(frame) = self.ctrl.latest_frame() {
            if self.last_seq != 0 && frame.seq > self.last_seq + 1 {
                self.dropped = self.dropped.saturating_add(frame.seq - self.last_seq - 1);
            }
            let nearest = self.integer_scaling;
            if frame.seq != self.last_seq {
                self.last_seq = frame.seq;
                self.upload_texture(ctx, &frame, nearest);
                if frame.key_c.iter().any(|value| value.is_finite()) {
                    self.last_keys = Some(frame.key_c);
                }
                if let Some(plane) = &frame.plane_c {
                    self.live_plane_stats = capture::plane_stats(plane);
                    self.live_plane = Some(Arc::new(plane.clone()));
                }
                self.last_frame = Some(frame.clone());
                self.confirm_palette_if_due(&frame);
            } else if self.texture_nearest != nearest
                && let Some(current) = self.last_frame.clone()
            {
                self.upload_texture(ctx, &current, nearest);
            }
        }
    }

    fn pump_usb_events(&mut self, ctx: &egui::Context) {
        let events = self
            .usb_worker
            .as_ref()
            .map(UsbWorker::try_events)
            .unwrap_or_default();
        for event in events {
            match event {
                UsbEvent::Volumes(volumes) => self.usb_volumes = volumes,
                UsbEvent::Status(status) => self.usb_status = status,
                UsbEvent::Imported(imported) => {
                    let imported = *imported;
                    let sha = imported.entry.sha256.clone();
                    if let Some(rgba) = &imported.thumbnail_rgba {
                        let image =
                            egui::ColorImage::from_rgba_unmultiplied([FRAME_W, FRAME_H], rgba);
                        self.usb_textures.entry(sha.clone()).or_insert_with(|| {
                            ctx.load_texture(
                                format!("usb-{sha}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            )
                        });
                    }
                    if let Some(existing) = self
                        .usb_entries
                        .iter_mut()
                        .find(|entry| entry.entry.sha256 == sha)
                    {
                        *existing = imported;
                    } else {
                        self.usb_entries.push(imported);
                    }
                    self.usb_entries
                        .sort_by_key(|entry| std::cmp::Reverse(entry.entry.imported_at));
                }
                UsbEvent::Error(text) => self.push_log(Level::Error, text),
                UsbEvent::Log { level, text } => self.push_log(level, text),
            }
        }
    }

    /// Upload a frame with the filter that matches the current zoom mode: nearest
    /// neighbour keeps the burned-in OSD crisp at integer scale, linear is used
    /// for fractional fit.
    fn upload_texture(&mut self, ctx: &egui::Context, frame: &LiveFrame, nearest: bool) {
        let options = if nearest {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };
        let image = egui::ColorImage::from_rgba_unmultiplied([FRAME_W, FRAME_H], &frame.rgba);
        match self.texture.as_mut() {
            Some(texture) => texture.set(image, options),
            None => self.texture = Some(ctx.load_texture("uti260b-live", image, options)),
        }
        self.texture_nearest = nearest;
    }

    /// Palette changes produce no reply. Confirm against the second frame that
    /// arrives after the request and surface a mismatch instead of a silent no-op.
    fn confirm_palette_if_due(&mut self, frame: &LiveFrame) {
        if self.palette_requested.is_none() || self.palette_frames_left == 0 {
            return;
        }
        self.palette_frames_left -= 1;
        if self.palette_frames_left > 0 {
            return;
        }

        let signature = frame_signature(&frame.rgba);
        let (detected, distance) = nearest_palette(signature);
        self.palette_detected = Some((detected, distance));

        let Some(requested) = self.palette_requested else {
            return;
        };
        if distance <= PALETTE_MATCH_DISTANCE && equivalent(requested, detected) {
            self.push_log(
                Level::Info,
                format!(
                    "palette confirmed: +{} ({}, Δ{distance:.1})",
                    requested.index(),
                    requested.description()
                ),
            );
        } else {
            let msg = format!(
                "palette unconfirmed: requested +{} but frame resembles +{} (Δ{distance:.1})",
                requested.index(),
                detected.index()
            );
            self.push_log(Level::Error, msg.clone());
            self.toast(msg, true);
        }
        self.palette_frames_left = 0;
    }

    // -- actions ------------------------------------------------------------

    fn resolve_target(&mut self) -> Option<StreamConfig> {
        let ip_text = self.ip_text.trim();
        let ip = if ip_text.is_empty() || ip_text.eq_ignore_ascii_case("auto") {
            match self.config.resolve_ip() {
                Ok(ip) => ip,
                Err(err) => {
                    self.push_log(Level::Error, err.clone());
                    self.toast(err, true);
                    return None;
                }
            }
        } else {
            match ip_text.parse::<std::net::Ipv4Addr>() {
                Ok(ip) => ip,
                Err(_) => {
                    let msg = format!("invalid IPv4 address {ip_text:?}");
                    self.push_log(Level::Error, msg.clone());
                    self.toast(msg, true);
                    return None;
                }
            }
        };

        let port = match self.port_text.trim().parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                let msg = format!("invalid TCP port {:?}", self.port_text);
                self.push_log(Level::Error, msg.clone());
                self.toast(msg, true);
                return None;
            }
        };

        Some(StreamConfig {
            addr: std::net::SocketAddr::from((ip, port)),
            connect_timeout: self.config.connect_timeout(),
            read_timeout: self.config.read_timeout(),
            target_fps: self.target_fps,
        })
    }

    fn connect(&mut self) {
        match &self.mode {
            Mode::Live => {
                let Some(config) = self.resolve_target() else {
                    return;
                };
                self.stream_requested = true;
                self.ctrl.start(config);
            }
            Mode::Replay { .. } => {
                self.stream_requested = true;
                self.ctrl.start(StreamConfig {
                    addr: "127.0.0.1:0".parse().expect("literal socket address"),
                    connect_timeout: Duration::from_secs(1),
                    read_timeout: Duration::from_secs(1),
                    target_fps: self.target_fps,
                });
            }
        }
    }

    fn disconnect(&mut self) {
        self.stream_requested = false;
        self.ctrl.stop();
    }

    /// Send one whitelisted command. Unit changes queue a follow-up `init` so the
    /// reported unit is confirmed against the device (§3.2, §13.11).
    fn run_command(&mut self, command: Command) {
        if matches!(self.mode, Mode::Replay { .. }) {
            self.toast("commands are disabled in replay mode", true);
            return;
        }
        if self.state != ConnState::Streaming {
            self.toast("not streaming; command not sent", true);
            return;
        }

        match command {
            Command::ColorChange(palette) => {
                self.palette_requested = Some(palette);
                self.palette_detected = None;
                self.palette_frames_left = 2;
            }
            Command::MeasureUnit(unit) => {
                self.unit_requested = Some(unit);
                self.unit_pending = true;
            }
            Command::PhotoCap => {
                self.capture_pending = true;
                self.capture_pending_since = Some(Instant::now());
            }
            Command::Init | Command::VFrame | Command::HeartBack => {}
        }

        self.push_log(Level::Info, format!("→ {}", command.describe()));
        self.ctrl.run(command);

        if matches!(command, Command::MeasureUnit(_)) {
            self.push_log(Level::Info, "→ init (unit confirmation)".to_owned());
            self.ctrl.run(Command::Init);
        }
    }

    fn export(&mut self) {
        let Some(frame) = self.last_frame.clone() else {
            self.toast("no frame to export yet", true);
            return;
        };
        match write_exports(&self.config.out_dir, &frame) {
            Ok((png, yuv)) => {
                let msg = format!(
                    "wrote {} and {}",
                    png.display(),
                    yuv.file_name().unwrap_or_default().to_string_lossy()
                );
                self.push_log(Level::Info, msg.clone());
                self.toast(msg, false);
            }
            Err(err) => {
                self.push_log(Level::Error, err.clone());
                self.toast(err, true);
            }
        }
    }

    fn export_capture(&mut self, kind: CaptureExport) {
        let Some(capture) = self.capture.clone() else {
            self.toast("no capture to export", true);
            return;
        };
        let dir = self.config.out_dir.clone();
        if let Err(err) = fs::create_dir_all(&dir) {
            let msg = format!("cannot create {}: {err}", dir.display());
            self.push_log(Level::Error, msg.clone());
            self.toast(msg, true);
            return;
        }
        let stem = format!("uti260b_{:06}", self.last_seq);
        let path = match kind {
            CaptureExport::Temperature => dir.join(format!("{stem}_temp.f32.bin")),
            CaptureExport::Y16 => dir.join(format!("{stem}_y16.u16.bin")),
        };
        let result = match kind {
            CaptureExport::Temperature => capture.write_temperature_plane(&path),
            CaptureExport::Y16 => capture.write_y16_plane(&path),
        };
        match result {
            Ok(()) => {
                let msg = format!("wrote {}", path.display());
                self.push_log(Level::Info, msg.clone());
                self.toast(msg, false);
            }
            Err(err) => {
                let msg = format!("cannot write {}: {err}", path.display());
                self.push_log(Level::Error, msg.clone());
                self.toast(msg, true);
            }
        }
    }

    fn push_log(&mut self, level: Level, text: impl Into<String>) {
        if self.log.len() == LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry {
            at: self.started.elapsed().as_secs_f32(),
            level,
            text: text.into(),
        });
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), Instant::now(), error));
    }

    fn unit_symbol(&self) -> Option<&'static str> {
        self.init.as_ref().and_then(|init| init.unit_symbol())
    }

    fn palette_status(&self) -> String {
        let Some(requested) = self.palette_requested else {
            return "not changed by this client".to_owned();
        };
        match self.palette_detected {
            Some((detected, distance)) => format!(
                "requested +{}{detected_word} +{} (Δ{distance:.1})",
                requested.index(),
                detected.index(),
                detected_word = if detected == requested {
                    " · detected"
                } else {
                    " · frame looks like"
                }
            ),
            None => format!("requested +{} · awaiting frame", requested.index()),
        }
    }

    // -- panels -------------------------------------------------------------

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let live = self.state.is_active();
            let running = self.stream_requested && live;

            if running {
                if ui
                    .button("Disconnect")
                    .on_hover_text("Stop streaming (Space)")
                    .clicked()
                {
                    self.disconnect();
                }
            } else if ui
                .button("Connect")
                .on_hover_text("Open TCP 9527, run init, start the frame loop (Space)")
                .clicked()
            {
                self.connect();
            }

            match &self.mode {
                Mode::Live => {
                    ui.separator();
                    ui.label("IP");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ip_text)
                            .desired_width(130.0)
                            .hint_text("auto"),
                    );
                    ui.label("port");
                    ui.add(egui::TextEdit::singleline(&mut self.port_text).desired_width(56.0));
                }
                Mode::Replay { path } => {
                    ui.separator();
                    ui.weak(format!(
                        "replay: {}",
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    ));
                }
            }

            ui.separator();
            let previous_fps = self.target_fps;
            ui.add(
                egui::Slider::new(&mut self.target_fps, 1..=30)
                    .text("fps")
                    .integer(),
            );
            if self.target_fps != previous_fps {
                self.ctrl.set_fps(self.target_fps);
            }

            ui.separator();
            ui.checkbox(&mut self.integer_scaling, "Integer scaling")
                .on_hover_text("Integer physical-pixel scale keeps the burned-in OSD crisp");

            ui.separator();
            let have_frame = self.last_frame.is_some();
            if ui
                .add_enabled(have_frame, egui::Button::new("Export PNG + YUV"))
                .clicked()
            {
                self.export();
            }
        });

        if let Some((text, at, error)) = &self.toast {
            if at.elapsed().as_secs_f32() < TOAST_SECS {
                let color = if *error {
                    egui::Color32::LIGHT_RED
                } else {
                    egui::Color32::LIGHT_GREEN
                };
                ui.label(egui::RichText::new(text).color(color).small());
            } else {
                self.toast = None;
            }
        }
        ui.add_space(2.0);
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let color = match self.state {
                ConnState::Streaming => egui::Color32::from_rgb(120, 220, 120),
                ConnState::Reconnecting | ConnState::Initialising | ConnState::Connecting => {
                    egui::Color32::from_rgb(240, 200, 90)
                }
                _ => egui::Color32::GRAY,
            };
            status_dot(ui, color);
            ui.label(egui::RichText::new(self.state.label()));
            if !self.state_detail.is_empty() {
                ui.weak(&self.state_detail);
            }

            ui.separator();
            match &self.mode {
                Mode::Live => {
                    ui.monospace(format!("{}:{}", self.ip_text.trim(), self.port_text.trim()));
                    ui.separator();
                    ui.monospace(format!("rtt {:.0} ms", self.stats.rtt_ms));
                }
                Mode::Replay { .. } => {
                    ui.monospace("fixture replay");
                }
            }

            ui.separator();
            ui.monospace(format!("{:.1} fps", self.stats.fps));
            ui.separator();
            ui.monospace(format!("{} frames", self.stats.frames));
            ui.separator();
            ui.monospace(format!("{} dropped", self.dropped))
                .on_hover_text("Frames replaced before the UI painted them");
            if self.stats.last_length > 0 {
                ui.separator();
                ui.monospace(format!("last {} B", self.stats.last_length));
            }
            if let Some(frame) = &self.last_frame {
                let age = frame.received.elapsed().as_secs_f32();
                if age > 1.0 {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 200, 90),
                        format!("frame age {age:.1} s"),
                    );
                }
            }

            ui.separator();
            match &self.init {
                Some(init) => {
                    ui.monospace(format!(
                        "fw {} / {} / {}",
                        init.versions[0], init.versions[1], init.versions[2]
                    ));
                }
                None => {
                    ui.weak("no init yet");
                }
            }
        });
    }

    fn key_temps(&mut self, ui: &mut egui::Ui) {
        ui.heading("Key temperatures");
        let symbol = self.unit_symbol();
        egui::Grid::new("key-temps")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for (index, label) in ["A", "B", "C"].into_iter().enumerate() {
                    ui.monospace(label.to_string());
                    let value = self.last_frame.as_ref().and_then(|frame| frame.key(index));
                    match value {
                        Some(v) => {
                            let suffix = symbol.unwrap_or("?");
                            ui.monospace(format!("{v:7.2} {suffix}"));
                        }
                        None => {
                            ui.monospace(NO_FRAME);
                        }
                    }
                    ui.end_row();
                }
            });
        if symbol.is_none() && self.init.is_some() {
            ui.colored_label(
                egui::Color32::from_rgb(240, 200, 90),
                "unit cannot be trusted (spec §3.2)",
            );
        }
        ui.small(
            "Unlabelled header readings: not max/min/centre of the measurement plane \
             and their source region is unknown (spec §5.2, E4).",
        );
    }

    fn measurement_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Measurement");
        let Some(capture) = self.capture.clone() else {
            if self.usb_plane.is_some() {
                self.usb_measurement_panel(ui);
                return;
            }
            if let Some(stats) = self.live_plane_stats {
                row(
                    ui,
                    "source",
                    "live vFrame plane (192×256, every 11th frame)",
                );
                row(
                    ui,
                    "range",
                    &format!("{:.2} … {:.2} °C", stats.min, stats.max),
                );
                row(
                    ui,
                    "mean / σ",
                    &format!("{:.2} / {:.2} °C", stats.mean, stats.std_dev),
                );
                row(ui, "mapping", "display / 1.25, no flip");
                match self.sample {
                    Some(sample) => {
                        row(ui, "sample", &format!("{:.2} °C", sample.temperature_c));
                        row(
                            ui,
                            "coords",
                            &format!(
                                "display ({:.1}, {:.1}) → plane ({}, {})",
                                sample.display[0],
                                sample.display[1],
                                sample.plane[0],
                                sample.plane[1]
                            ),
                        );
                    }
                    None => {
                        ui.small("Click the image to sample the live temperature plane.");
                    }
                }
                ui.small(
                    "The camera appends this plane to every 11th vFrame; PhotoCap gives \
                     a full-resolution capture and matching JPEGs.",
                );
                return;
            }
            ui.weak(
                "No temperature plane yet. The device appends one to every 11th vFrame; \
                 PhotoCap gives a full-resolution capture (several seconds, no SD write).",
            );
            return;
        };
        let Some(stats) = self.capture_stats else {
            return;
        };

        row(ui, "plane", "192 × 256 f32 LE °C (stride 192)");
        row(
            ui,
            "range",
            &format!("{:.2} … {:.2} °C", stats.min, stats.max),
        );
        row(
            ui,
            "mean / σ",
            &format!("{:.2} / {:.2} °C", stats.mean, stats.std_dev),
        );
        row(ui, "mapping", "display / 1.25, no flip");
        row(ui, "shot JPEG", &format!("{} B", capture.shot_jpeg.len()));
        row(ui, "vis JPEG", &format!("{} B", capture.vis_jpeg.len()));
        row(ui, "y16 plane", &format!("{} B", capture.y16.len()));
        row(
            ui,
            "tail [U]",
            &capture
                .tail
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        row(
            ui,
            "age",
            &format!("{:.1} s", capture.received.elapsed().as_secs_f32()),
        );

        match self.sample {
            Some(sample) => {
                row(ui, "sample", &format!("{:.2} °C", sample.temperature_c));
                row(
                    ui,
                    "coords",
                    &format!(
                        "display ({:.1}, {:.1}) → plane ({}, {})",
                        sample.display[0], sample.display[1], sample.plane[0], sample.plane[1]
                    ),
                );
            }
            None => {
                ui.small("Click the image to sample the temperature plane.");
            }
        }

        ui.horizontal(|ui| {
            if ui.button("Export temp plane").clicked() {
                self.export_capture(CaptureExport::Temperature);
            }
            if ui.button("Export y16 plane").clicked() {
                self.export_capture(CaptureExport::Y16);
            }
        });
        ui.small(
            "Section 0 is the OSD JPEG, section 2 the clean 480×640 JPEG. The u16 plane \
             is not linear in temperature; it is kept for diagnostics only (spec §6.1).",
        );
    }

    fn usb_measurement_panel(&mut self, ui: &mut egui::Ui) {
        let Some(stats) = self.usb_stats else {
            return;
        };
        let selected = self.usb_selected.clone();
        let (name, metadata) = selected
            .as_ref()
            .and_then(|sha| {
                self.usb_entries
                    .iter()
                    .find(|entry| &entry.entry.sha256 == sha)
                    .map(|entry| {
                        (
                            entry.entry.source_name.clone(),
                            entry.entry.metadata.clone(),
                        )
                    })
            })
            .unwrap_or_else(|| ("USB import".to_owned(), UsbMetadata::default()));

        row(ui, "source", &format!("USB import · {name} (read-only)"));
        row(ui, "firmware", &metadata.firmware);
        row(ui, "model", &metadata.model);
        row(
            ui,
            "emissivity",
            &metadata
                .emissivity
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "—".to_owned()),
        );
        row(
            ui,
            "distance",
            &metadata
                .distance_m
                .map(|value| format!("{value:.2} m"))
                .unwrap_or_else(|| "—".to_owned()),
        );
        row(
            ui,
            "humidity",
            &metadata
                .humidity_pct
                .map(|value| format!("{value} %RH"))
                .unwrap_or_else(|| "—".to_owned()),
        );
        row(
            ui,
            "ambient",
            &metadata
                .ambient
                .map(|value| format!("{value:.1}"))
                .unwrap_or_else(|| "—".to_owned()),
        );
        row(
            ui,
            "unit",
            &metadata
                .unit
                .map(|value| format!("byte {value}"))
                .unwrap_or_else(|| "—".to_owned()),
        );
        row(
            ui,
            "range",
            &format!("{:.2} … {:.2} °C", stats.min, stats.max),
        );
        row(
            ui,
            "mean / σ",
            &format!("{:.2} / {:.2} °C", stats.mean, stats.std_dev),
        );
        row(ui, "mapping", "display / 1.25, no flip");
        match self.sample {
            Some(sample) => {
                row(ui, "sample", &format!("{:.2} °C", sample.temperature_c));
                row(
                    ui,
                    "coords",
                    &format!(
                        "display ({:.1}, {:.1}) → plane ({}, {})",
                        sample.display[0], sample.display[1], sample.plane[0], sample.plane[1]
                    ),
                );
            }
            None => {
                ui.small("Click the image to sample the imported plane.");
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Export USB plane").clicked() {
                self.export_usb_plane();
            }
            if ui.button("Use live plane").clicked() {
                self.clear_usb_selection();
            }
        });
        ui.small("Imported files are copied to a host cache; the camera volume is never written.");
    }

    fn usb_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("USB import");
        ui.small(
            "Read-only: never write, rename, delete or repair the camera volume. Selecting USB \
             mode on the camera may drop its access point.",
        );
        let status = self.usb_status.clone();
        let volumes = self.usb_volumes.clone();
        ui.horizontal(|ui| {
            if ui.button("Rescan").clicked()
                && let Some(worker) = &self.usb_worker
            {
                worker.rescan();
            }
            ui.weak(status);
        });
        for volume in &volumes {
            ui.small(volume);
        }
        if self.usb_entries.is_empty() {
            ui.weak(
                "No imported files yet. Mount the camera volume read-only; imports are keyed by \
                 content hash.",
            );
            return;
        }

        ui.small("Click a thumbnail to use its plane for measurement.");
        let mut clicked = None;
        for index in 0..self.usb_entries.len() {
            let (sha, name, firmware, min, max, selected) = {
                let entry = &self.usb_entries[index];
                (
                    entry.entry.sha256.clone(),
                    entry.entry.source_name.clone(),
                    entry.entry.metadata.firmware.clone(),
                    entry.entry.min_c,
                    entry.entry.max_c,
                    self.usb_selected.as_deref() == Some(entry.entry.sha256.as_str()),
                )
            };
            let texture = self.usb_textures.get(&sha).cloned();
            ui.horizontal(|ui| {
                if let Some(texture) = texture {
                    ui.add(egui::Image::new(&texture).fit_to_exact_size(egui::vec2(40.0, 53.0)));
                } else {
                    ui.add_sized([40.0, 53.0], egui::Label::new("—"));
                }
                ui.vertical(|ui| {
                    if ui.selectable_label(selected, name).clicked() {
                        clicked = Some(sha.clone());
                    }
                    ui.small(format!("{firmware} · {min:.1}…{max:.1} °C"));
                });
            });
        }
        if let Some(sha) = clicked {
            self.select_usb(&sha);
        }
    }

    fn select_usb(&mut self, sha: &str) {
        let Some(entry) = self
            .usb_entries
            .iter()
            .find(|entry| entry.entry.sha256 == sha)
        else {
            return;
        };
        let plane = Arc::clone(&entry.plane);
        let stats = TempStats {
            min: entry.entry.min_c,
            max: entry.entry.max_c,
            mean: entry.entry.mean_c,
            std_dev: entry.entry.std_dev_c,
        };
        let metadata = entry.entry.metadata.clone();
        let name = entry.entry.source_name.clone();
        self.usb_selected = Some(sha.to_owned());
        self.usb_plane = Some(plane);
        self.usb_stats = Some(stats);
        self.usb_meta = Some(metadata);
        self.capture = None;
        self.capture_stats = None;
        self.sample = None;
        self.push_log(Level::Info, format!("USB measurement source: {name}"));
    }

    fn clear_usb_selection(&mut self) {
        self.usb_selected = None;
        self.usb_plane = None;
        self.usb_stats = None;
        self.usb_meta = None;
        self.sample = None;
    }

    fn export_usb_plane(&mut self) {
        let Some(plane) = self.usb_plane.clone() else {
            self.toast("no USB plane selected", true);
            return;
        };
        let dir = self.config.out_dir.clone();
        if let Err(err) = fs::create_dir_all(&dir) {
            let msg = format!("cannot create {}: {err}", dir.display());
            self.push_log(Level::Error, msg.clone());
            self.toast(msg, true);
            return;
        }
        let sha = self.usb_selected.as_deref().unwrap_or("usb");
        let stem = &sha[..12.min(sha.len())];
        let path = dir.join(format!("usb_{stem}_plane.f32"));
        let mut bytes = Vec::with_capacity(plane.len() * 4);
        for value in plane.iter() {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        match fs::write(&path, bytes) {
            Ok(()) => {
                let msg = format!("wrote {}", path.display());
                self.push_log(Level::Info, msg.clone());
                self.toast(msg, false);
            }
            Err(err) => {
                let msg = format!("cannot write {}: {err}", path.display());
                self.push_log(Level::Error, msg.clone());
                self.toast(msg, true);
            }
        }
    }

    fn device_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device state");
        let Some(init) = self.init.as_ref() else {
            ui.weak("No init reply yet — connect to read the device state.");
            return;
        };

        let marks = init.mark_flags;
        let mark = |bit: u32| if marks & (1 << bit) != 0 { "on" } else { "off" };
        let (scale_a, scale_b) = init.color_scale_floats();
        let palette = self.palette_status();

        let symbol = init.unit_symbol().unwrap_or("?");
        row(ui, "versions", &init.versions.join(" / "));
        row(ui, "emissivity", &format!("{:.2}", init.emissivity));
        row(ui, "distance", &format!("{:.2} m", init.distance_m));
        row(
            ui,
            "ambient / reflected",
            &format!("{:.1} {symbol}", init.ambient_c),
        );
        row(
            ui,
            "second ambient [U]",
            &format!("{:.1} {symbol}", init.ambient2_c),
        );
        row(ui, "humidity", &format!("{} %RH", init.humidity_pct));
        row(
            ui,
            "ambient int [I]",
            &format!("{} {symbol}", init.ambient_int_c),
        );
        row(
            ui,
            "unit",
            &format!("{} (byte {})", init.unit_label(), init.unit),
        );
        row(ui, "palette", &palette);
        row(
            ui,
            "alarm",
            &format!(
                "{} · Δ {:.1} K · {:.1} {symbol}",
                if init.alarm_enable != 0 { "on" } else { "off" },
                init.alarm_high_delta_k,
                init.alarm_value_c
            ),
        );
        row(
            ui,
            "marks [I]",
            &format!(
                "0x{marks:02x} · c:{} x:{} n:{} d:{}",
                mark(0),
                mark(1),
                mark(2),
                mark(3)
            ),
        );
        row(ui, "imageSelect [I]", &format!("{}", init.image_select));
        row(
            ui,
            "PiP rect",
            &format!(
                "x{} y{} {}x{}",
                init.pip_rect[0], init.pip_rect[1], init.pip_rect[2], init.pip_rect[3]
            ),
        );
        row(ui, "fusion level [I]", &format!("{:.2}", init.fusion_level));
        row(
            ui,
            "colour scale [U]",
            &format!("f32@+8 {scale_a:.2} · +12 {scale_b:.2}"),
        );
        row(
            ui,
            "camera info [U]",
            &format!("{}, {}", init.camera_info[0], init.camera_info[1]),
        );
        row(
            ui,
            "other info [U]",
            &format!("{}, {}", init.other_info[0], init.other_info[1]),
        );
        row(
            ui,
            "laser px [U]",
            &format!("{}, {}", init.laser_px[0], init.laser_px[1]),
        );
        row(
            ui,
            "fuse XY [U]",
            &format!(
                "{}, {}, {}",
                init.fuse_xy[0], init.fuse_xy[1], init.fuse_xy[2]
            ),
        );
        row(ui, "colour scale raw", &hex_string(&init.color_scale));
        row(ui, "raw payload", &format!("{} B", init.raw.len()));

        ui.horizontal(|ui| {
            if ui.small_button("copy colour scale").clicked() {
                ui.ctx().copy_text(hex_string(&init.color_scale));
            }
            if ui.small_button("copy init hex").clicked() {
                ui.ctx().copy_text(hex_string(&init.raw));
            }
        });

        ui.small(
            "Fields marked [U] have unknown semantics and are shown raw; \
             [I] fields are inferred from client behaviour (spec §4).",
        );
    }

    fn command_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Commands");
        ui.small(
            "Typed whitelist only: the device daemon exits on a malformed command and \
             needs a power cycle (spec §3). Only verified bare commands and verified \
             arguments are exposed here.",
        );

        let can_send = self.state == ConnState::Streaming && matches!(self.mode, Mode::Live);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_send, egui::Button::new("Refresh init"))
                .on_hover_text("Re-read device state; also confirms unit changes")
                .clicked()
            {
                self.run_command(Command::Init);
            }

            let capture_enabled = !self.capture_pending && can_send;
            let capture_label = if self.capture_pending {
                "Capturing…"
            } else {
                "PhotoCap…"
            };
            if ui
                .add_enabled(capture_enabled, egui::Button::new(capture_label))
                .on_hover_text(
                    "Full-resolution capture: several seconds, no SD-card write, returns the \
                     192×256 f32 temperature plane; serialised client-side",
                )
                .clicked()
            {
                self.toast("PhotoCap requested (~3 s)", false);
                self.run_command(Command::PhotoCap);
            }
        });

        ui.add_space(4.0);
        ui.label("Palette · colorChange+0..7");
        ui.add_enabled_ui(can_send, |ui| {
            ui.horizontal_wrapped(|ui| {
                for palette in Palette::ALL {
                    let selected = self.palette_requested == Some(palette);
                    if ui
                        .selectable_label(selected, palette.index().to_string())
                        .on_hover_text(palette.description())
                        .clicked()
                    {
                        self.run_command(Command::ColorChange(palette));
                    }
                }
            });
        });
        if self.palette_requested.is_some() {
            ui.small(self.palette_status());
        }

        ui.add_space(4.0);
        ui.label("Unit · measUnit");
        let current_unit = self.init.as_ref().map(|init| init.unit_label());
        ui.add_enabled_ui(can_send, |ui| {
            ui.horizontal_wrapped(|ui| {
                for unit in UnitCommand::ALL {
                    if ui
                        .button(unit.label())
                        .on_hover_text("Verified argument pair; confirmed by re-reading init")
                        .clicked()
                    {
                        self.run_command(Command::MeasureUnit(unit));
                    }
                }
            });
        });
        match current_unit {
            Some(unit) => {
                ui.small(format!("init reports unit: {unit}"));
            }
            None => {
                ui.small("unit unknown until init succeeds");
            }
        }
        if self.unit_pending {
            ui.small("waiting for init to confirm the unit change…");
        }

        ui.small(
            "Unverified commands (imgModel, markTool, scaleChange, scaleUpdata, pipRectMove, \
             measParam, alert, brig, tf, rtc, lang, photoModel) are intentionally not exposed.",
        );
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Log")
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .stick_to_bottom(true)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for entry in &self.log {
                            let color = match entry.level {
                                Level::Info => egui::Color32::from_gray(190),
                                Level::Warn => egui::Color32::from_rgb(230, 200, 100),
                                Level::Error => egui::Color32::LIGHT_RED,
                            };
                            ui.label(
                                egui::RichText::new(format!("{:7.2}s {}", entry.at, entry.text))
                                    .monospace()
                                    .small()
                                    .color(color),
                            );
                        }
                    });
            });
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        self.key_temps(ui);
        ui.separator();
        self.measurement_panel(ui);
        ui.separator();
        self.usb_panel(ui);
        ui.separator();
        self.device_panel(ui);
        ui.separator();
        self.command_panel(ui);
        ui.separator();
        self.log_panel(ui);
        ui.separator();
        ui.collapsing("About", |ui| {
            ui.small(
                "Independent UTi260B+ client. Live view frames are palette-mapped \
                 240×320 YUV with three unlabelled header temperatures. Per-pixel \
                 measurement uses PhotoCap, the live plane, or a read-only USB import; all \
                 planes are 192×256 and map to the display at exactly 1.25.",
            );
            ui.small(format!(
                "Frame payload: {} B pixels + 12 B temperatures; TCP {}.",
                protocol::YUV_LEN,
                self.config.live_port
            ));
            if let Some(source) = &self.config.source {
                ui.small(format!("Config: {}", source.display()));
            }
        });
    }

    /// Central area split: live image on the left, inspector on the right.
    /// A plain horizontal split is used instead of a right-hand panel so the
    /// inspector's layout can never overlap the image when the window is narrow.
    fn body(&mut self, ui: &mut egui::Ui) {
        let total_width = ui.available_width();
        let height = ui.available_height();
        let spacing = 10.0;
        let inspector_width = 380.0_f32.min((total_width * 0.55).max(220.0));
        let viewer_width = (total_width - inspector_width - spacing).max(120.0);

        ui.horizontal_top(|ui| {
            // Distinct id scopes: two sibling allocated Uis would otherwise
            // collide on egui's auto ids.
            ui.push_id("viewer-pane", |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(viewer_width, height),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| self.viewer(ui),
                );
            });
            ui.separator();
            ui.push_id("inspector-pane", |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(inspector_width, height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_max_width(inspector_width);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.inspector(ui));
                    },
                );
            });
        });
    }

    fn viewer(&mut self, ui: &mut egui::Ui) {
        let usb_texture = self
            .usb_selected
            .as_ref()
            .and_then(|sha| self.usb_textures.get(sha))
            .cloned();
        let Some(texture) = usb_texture.or_else(|| self.texture.clone()) else {
            ui.centered_and_justified(|ui| {
                ui.weak("No frame yet");
            });
            return;
        };

        let available = ui.available_size();
        let base = egui::vec2(FRAME_W as f32, FRAME_H as f32);
        // Integer scaling is computed in physical pixels, then converted back to
        // points, so the OSD is crisp even on a fractional-DPI display.
        let ppp = ui.ctx().pixels_per_point().max(0.1);
        let available_px = available * ppp;
        let fit_px = (available_px.x / base.x)
            .min(available_px.y / base.y)
            .max(0.05);
        let scale_px = if self.integer_scaling {
            fit_px.floor().max(1.0)
        } else {
            fit_px
        };
        let size = base * (scale_px / ppp);

        let capture = self.capture.clone();
        let has_live_plane = self.live_plane.is_some();
        let has_plane_source = self.capture.is_some() || self.usb_plane.is_some() || has_live_plane;
        let sample = self.sample;
        let mut clicked_display: Option<[f32; 2]> = None;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let response = ui.add(
                        egui::Image::new(&texture)
                            .fit_to_exact_size(size)
                            .sense(egui::Sense::click()),
                    );
                    let rect = response.rect;

                    if response.clicked()
                        && rect.width() > 0.0
                        && (capture.is_some() || has_plane_source)
                        && let Some(pos) = response.interact_pointer_pos()
                    {
                        let relative = (pos - rect.min) / rect.size();
                        clicked_display = Some([
                            relative.x.clamp(0.0, 1.0) * FRAME_W as f32,
                            relative.y.clamp(0.0, 1.0) * FRAME_H as f32,
                        ]);
                    }

                    if let Some(sample) = sample {
                        let point = rect.min
                            + egui::vec2(
                                sample.display[0] / FRAME_W as f32 * rect.width(),
                                sample.display[1] / FRAME_H as f32 * rect.height(),
                            );
                        let color = egui::Color32::from_rgb(255, 220, 80);
                        let painter = ui.painter();
                        painter.circle_stroke(point, 6.0, egui::Stroke::new(1.5, color));
                        painter.line_segment(
                            [point - egui::vec2(11.0, 0.0), point + egui::vec2(11.0, 0.0)],
                            egui::Stroke::new(1.0, color),
                        );
                        painter.line_segment(
                            [point - egui::vec2(0.0, 11.0), point + egui::vec2(0.0, 11.0)],
                            egui::Stroke::new(1.0, color),
                        );
                        painter.text(
                            point + egui::vec2(10.0, -8.0),
                            egui::Align2::LEFT_BOTTOM,
                            format!("{:.2} °C", sample.temperature_c),
                            egui::FontId::monospace(11.0),
                            egui::Color32::WHITE,
                        );
                    }
                });
            });

        if let Some(display) = clicked_display
            && let Some((px, py)) = capture::plane_coords(display[0], display[1])
        {
            let temperature_c = if let Some(capture) = self.capture.as_ref() {
                capture.temperature_at_display(display[0], display[1])
            } else if let Some(plane) = self.usb_plane.as_deref() {
                capture::sample_plane(plane, display[0], display[1])
            } else {
                self.live_plane
                    .as_deref()
                    .and_then(|plane| capture::sample_plane(plane, display[0], display[1]))
            };
            if let Some(temperature_c) = temperature_c {
                self.sample = Some(Sample {
                    display,
                    plane: [px, py],
                    temperature_c,
                });
            }
        }

        if let Some(frame) = &self.last_frame {
            let source = if self.usb_plane.is_some() {
                " · USB import"
            } else if self.capture.is_some() || self.live_plane_stats.is_some() {
                " · click to sample"
            } else {
                " · waiting for a temperature plane"
            };
            ui.horizontal(|ui| {
                ui.small(format!(
                    "frame {} · {:.0} ms round trip · {}×{} at {scale_px:.2}×{source}",
                    frame.seq,
                    frame.rtt.as_secs_f32() * 1000.0,
                    FRAME_W,
                    FRAME_H
                ));
            });
        }
    }
}

impl eframe::App for CameraApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump_events(ui.ctx());

        let toggle = ui.input(|i| i.key_pressed(egui::Key::Space));
        if toggle && !ui.ctx().egui_wants_keyboard_input() {
            if self.stream_requested {
                self.disconnect();
            } else {
                self.connect();
            }
        }

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.body(ui));

        let repaint = if self.state.is_active() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
        };
        ui.ctx().request_repaint_after(repaint);
    }
}

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.weak(label);
        ui.add(egui::Label::new(egui::RichText::new(value).monospace()).wrap());
    });
}

fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn hex_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn write_exports(
    out_dir: &std::path::Path,
    frame: &LiveFrame,
) -> Result<(PathBuf, PathBuf), String> {
    fs::create_dir_all(out_dir).map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;

    let stem = format!("uti260b_{:06}", frame.seq);
    let png_path = out_dir.join(format!("{stem}.png"));
    let yuv_path = out_dir.join(format!("{stem}.yuv"));

    {
        let file = fs::File::create(&png_path)
            .map_err(|e| format!("cannot create {}: {e}", png_path.display()))?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), FRAME_W as u32, FRAME_H as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("writing {}: {e}", png_path.display()))?;
        writer
            .write_image_data(&frame.rgba)
            .map_err(|e| format!("writing {}: {e}", png_path.display()))?;
    }

    fs::write(&yuv_path, &frame.yuv).map_err(|e| format!("writing {}: {e}", yuv_path.display()))?;

    Ok((png_path, yuv_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_png_and_raw_yuv() {
        let dir = std::env::temp_dir().join(format!(
            "uti260b-export-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let frame = LiveFrame::decode(
            &crate::fixtures::yuv_frame(),
            crate::fixtures::SAMPLE_KEY_C,
            None,
            7,
            Duration::from_millis(42),
        );

        let (png, yuv) = write_exports(&dir, &frame).expect("export");
        let png_bytes = fs::read(&png).expect("read png");
        assert_eq!(&png_bytes[1..4], b"PNG");
        assert_eq!(fs::read(&yuv).expect("read yuv").len(), protocol::YUV_LEN);
        assert!(
            png.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("000007")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_frame_key_is_none() {
        let mut frame = LiveFrame::decode(
            &crate::fixtures::yuv_frame(),
            [f32::NAN; 3],
            None,
            1,
            Duration::ZERO,
        );
        assert!(frame.key(0).is_none());
        frame.key_c = [1.0, 2.0, 3.0];
        assert_eq!(frame.key(1), Some(2.0));
    }
}
