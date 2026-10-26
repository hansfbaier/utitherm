//! Network worker: owns the TCP connection to `camera:9527`, runs the session
//! state machine (§7), publishes the newest decoded frame and handles the typed
//! command whitelist (§3).
//!
//! The UI never blocks on the socket. Frames are handed over with
//! [`arc_swap::ArcSwap`] so the worker can always overwrite a frame the UI has
//! not painted yet; queueing is deliberately avoided (§8.1).

use std::collections::VecDeque;
use std::io::{self, Write as _};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, unbounded};
use socket2::{SockRef, TcpKeepalive};

use crate::capture::Capture;
use crate::frame::LiveFrame;
use crate::protocol::{self, Command, INIT_MESSAGE_LEN, InitReply, ProtoError};

/// Reconnect backoff schedule (§7).
const BACKOFF_MS: [u64; 4] = [250, 500, 1000, 2000];
/// How often stats are pushed to the UI.
const STATS_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound on a single control-wait, keeps stop/shutdown responsive.
const CONTROL_POLL: Duration = Duration::from_millis(100);
/// `PhotoCap` takes ~3.3 s on the device (§6); the frame timeout would be too short.
const PHOTOCAP_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Commands waiting for the current frame slot.
const COMMAND_QUEUE_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    Idle,
    Connecting,
    Initialising,
    Streaming,
    Reconnecting,
    Stopped,
}

impl ConnState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Connecting => "connecting",
            Self::Initialising => "initialising",
            Self::Streaming => "streaming",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Initialising | Self::Streaming | Self::Reconnecting
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StreamConfig {
    pub addr: SocketAddr,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub target_fps: u32,
}

impl StreamConfig {
    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.target_fps.clamp(1, 60)))
    }
}

#[derive(Debug)]
pub enum WorkerMsg {
    Start(Box<StreamConfig>),
    Stop,
    SetFps(u32),
    Run(Command),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NetStats {
    pub rtt_ms: f32,
    pub fps: f32,
    pub frames: u64,
    pub last_length: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: Level,
    pub text: String,
}

#[derive(Debug)]
pub enum Event {
    State {
        state: ConnState,
        detail: String,
    },
    Init(Box<InitReply>),
    Stats(NetStats),
    /// Full-resolution capture with the 192x256 temperature plane.
    Capture(Box<Capture>),
    Log(LogLine),
}

pub struct Worker {
    tx: Sender<WorkerMsg>,
    events: Receiver<Event>,
    latest: Arc<ArcSwap<Option<Arc<LiveFrame>>>>,
    stop: Arc<AtomicBool>,
    socket_slot: Arc<Mutex<Option<TcpStream>>>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn() -> io::Result<Self> {
        let (tx, rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let latest = Arc::new(ArcSwap::from_pointee(None::<Arc<LiveFrame>>));
        let stop = Arc::new(AtomicBool::new(false));
        let socket_slot = Arc::new(Mutex::new(None::<TcpStream>));

        let join = {
            let latest = Arc::clone(&latest);
            let stop = Arc::clone(&stop);
            let socket_slot = Arc::clone(&socket_slot);
            thread::Builder::new()
                .name("uti260b-net".into())
                .spawn(move || Runtime::new(rx, event_tx, latest, stop, socket_slot).run())?
        };

        Ok(Self {
            tx,
            events: event_rx,
            latest,
            stop,
            socket_slot,
            join: Some(join),
        })
    }

    pub fn start(&self, config: StreamConfig) {
        let _ = self.tx.send(WorkerMsg::Start(Box::new(config)));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(WorkerMsg::Stop);
    }

    pub fn set_fps(&self, fps: u32) {
        let _ = self.tx.send(WorkerMsg::SetFps(fps));
    }

    /// Queue one whitelisted command. The caller can only pass [`Command`].
    pub fn run(&self, command: Command) {
        let _ = self.tx.send(WorkerMsg::Run(command));
    }

    pub fn latest_frame(&self) -> Option<Arc<LiveFrame>> {
        self.latest.load().as_ref().as_ref().map(Arc::clone)
    }

    pub fn try_events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(WorkerMsg::Shutdown);
        // Unblock a read_exact in the worker without waiting for its timeout.
        if let Ok(slot) = self.socket_slot.lock()
            && let Some(stream) = slot.as_ref()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker runtime
// ---------------------------------------------------------------------------

struct Conn {
    stream: TcpStream,
    reader: io::BufReader<TcpStream>,
    next_frame_at: Instant,
}

struct StatsAcc {
    frames: u64,
    rtt_ms: f32,
    window_start: Instant,
    window_frames: u32,
    fps: f32,
    last_length: u32,
    last_emit: Instant,
}

impl StatsAcc {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            frames: 0,
            rtt_ms: 0.0,
            window_start: now,
            window_frames: 0,
            fps: 0.0,
            last_length: 0,
            last_emit: now,
        }
    }

    fn record(&mut self, rtt: Duration, length: u32) {
        self.frames = self.frames.saturating_add(1);
        self.window_frames = self.window_frames.saturating_add(1);
        self.last_length = length;

        let rtt_ms = rtt.as_secs_f32() * 1000.0;
        self.rtt_ms = if self.rtt_ms == 0.0 {
            rtt_ms
        } else {
            self.rtt_ms * 0.8 + rtt_ms * 0.2
        };

        let window = self.window_start.elapsed();
        if window >= Duration::from_secs(1) {
            self.fps = self.window_frames as f32 / window.as_secs_f32();
            self.window_start = Instant::now();
            self.window_frames = 0;
        }
    }

    fn snapshot(&self) -> NetStats {
        NetStats {
            rtt_ms: self.rtt_ms,
            fps: self.fps,
            frames: self.frames,
            last_length: self.last_length,
        }
    }
}

struct Runtime {
    rx: Receiver<WorkerMsg>,
    tx: Sender<Event>,
    latest: Arc<ArcSwap<Option<Arc<LiveFrame>>>>,
    stop: Arc<AtomicBool>,
    socket_slot: Arc<Mutex<Option<TcpStream>>>,

    config: Option<StreamConfig>,
    running: bool,
    state: ConnState,
    detail: String,
    backoff: usize,
    next_attempt: Instant,
    seq: u64,
    pending: VecDeque<Command>,
    stats: StatsAcc,
    conn: Option<Conn>,
    /// Extended frames (with live plane) seen in this session.
    saw_live_plane: bool,
    /// Consecutive failures; logged to make a desynchronised link obvious.
    consecutive_failures: u32,
}

impl Runtime {
    fn new(
        rx: Receiver<WorkerMsg>,
        tx: Sender<Event>,
        latest: Arc<ArcSwap<Option<Arc<LiveFrame>>>>,
        stop: Arc<AtomicBool>,
        socket_slot: Arc<Mutex<Option<TcpStream>>>,
    ) -> Self {
        let state = ConnState::Idle;
        let detail = "waiting for connect".to_owned();
        let _ = tx.send(Event::State {
            state,
            detail: detail.clone(),
        });
        Self {
            rx,
            tx,
            latest,
            stop,
            socket_slot,
            config: None,
            running: false,
            state,
            detail,
            backoff: 0,
            next_attempt: Instant::now(),
            seq: 0,
            pending: VecDeque::new(),
            stats: StatsAcc::new(),
            conn: None,
            saw_live_plane: false,
            consecutive_failures: 0,
        }
    }

    fn run(mut self) {
        while !self.stop.load(Ordering::SeqCst) {
            self.drain_control();
            if self.stop.load(Ordering::SeqCst) {
                break;
            }

            if !self.running {
                // Nothing to poll while idle: block until Start/Shutdown arrives.
                match self.rx.recv() {
                    Ok(msg) => self.handle(msg),
                    Err(_) => break,
                }
                continue;
            }

            if self.conn.is_none() {
                if Instant::now() < self.next_attempt {
                    let wait = self
                        .next_attempt
                        .saturating_duration_since(Instant::now())
                        .min(CONTROL_POLL);
                    self.wait_control(wait);
                    continue;
                }
                self.open();
                continue;
            }

            if let Some(command) = self.pending.pop_front() {
                self.run_command(command);
                continue;
            }

            let Some(conn) = self.conn.as_ref() else {
                continue;
            };
            if Instant::now() < conn.next_frame_at {
                let wait = conn
                    .next_frame_at
                    .saturating_duration_since(Instant::now())
                    .min(CONTROL_POLL);
                self.wait_control(wait);
                continue;
            }

            self.run_frame();
        }

        self.disconnect();
        self.set_state(ConnState::Stopped, "worker shut down");
    }

    // -- control ------------------------------------------------------------

    fn drain_control(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    fn wait_control(&mut self, duration: Duration) {
        if duration.is_zero() {
            return;
        }
        match self.rx.recv_timeout(duration) {
            Ok(msg) => self.handle(msg),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                self.stop.store(true, Ordering::SeqCst);
            }
        }
    }

    fn handle(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Start(config) => {
                let changed = self.config.as_ref().is_some_and(|old| {
                    old.addr != config.addr || old.read_timeout != config.read_timeout
                });
                if changed || self.conn.is_some() {
                    self.disconnect();
                }
                self.log(
                    Level::Info,
                    format!(
                        "connect to {} (target {} fps, timeouts {} / {} ms)",
                        config.addr,
                        config.target_fps,
                        config.connect_timeout.as_millis(),
                        config.read_timeout.as_millis()
                    ),
                );
                self.config = Some(*config);
                self.running = true;
                self.backoff = 0;
                self.next_attempt = Instant::now();
                self.set_state(ConnState::Connecting, "opening TCP connection");
            }
            WorkerMsg::Stop => {
                self.running = false;
                self.pending.clear();
                self.disconnect();
                self.zero_stats();
                self.set_state(ConnState::Stopped, "stopped by user");
                self.log(Level::Info, "streaming stopped".to_owned());
            }
            WorkerMsg::SetFps(fps) => {
                let fps = fps.clamp(1, 60);
                if let Some(config) = self.config.as_mut() {
                    config.target_fps = fps;
                }
                self.log(Level::Info, format!("target fps set to {fps}"));
            }
            WorkerMsg::Run(command) => {
                if !self.running || self.conn.is_none() {
                    self.log(
                        Level::Warn,
                        format!("not connected; {} was not sent", command.describe()),
                    );
                    return;
                }
                if self.pending.len() >= COMMAND_QUEUE_LEN {
                    self.log(
                        Level::Warn,
                        format!("command queue full; {} dropped", command.describe()),
                    );
                    return;
                }
                self.pending.push_back(command);
            }
            WorkerMsg::Shutdown => {
                self.stop.store(true, Ordering::SeqCst);
                self.running = false;
            }
        }
    }

    // -- session ------------------------------------------------------------

    fn open(&mut self) {
        let Some(config) = self.config else {
            self.running = false;
            return;
        };

        self.set_state(
            ConnState::Connecting,
            format!("connecting to {}", config.addr),
        );

        let stream = match connect_socket(&config) {
            Ok(stream) => stream,
            Err(err) => {
                self.schedule_retry(format!("connect to {} failed: {err}", config.addr));
                return;
            }
        };

        if let Ok(clone) = stream.try_clone()
            && let Ok(mut slot) = self.socket_slot.lock()
        {
            *slot = Some(clone);
        }

        let Ok(reader_stream) = stream.try_clone() else {
            self.schedule_retry("cannot clone the socket handle for the reader".into());
            return;
        };
        let reader = io::BufReader::with_capacity(16 * 1024, reader_stream);
        let mut conn = Conn {
            stream,
            reader,
            next_frame_at: Instant::now(),
        };

        self.set_state(ConnState::Initialising, "sending init");
        let started = Instant::now();
        match exchange_command(&mut conn, Command::Init) {
            Ok(message) if message.length != INIT_MESSAGE_LEN => {
                self.schedule_retry(format!(
                    "init reply is {} B, expected {INIT_MESSAGE_LEN}",
                    message.length
                ));
            }
            Ok(message) => match InitReply::parse(&message.payload) {
                Ok(reply) => {
                    let elapsed = started.elapsed();
                    self.log(
                        Level::Info,
                        format!(
                            "init ok: {} / {} / {} ({} ms, {} B, unit {})",
                            reply.versions[0],
                            reply.versions[1],
                            reply.versions[2],
                            elapsed.as_millis(),
                            message.length,
                            reply.unit_label()
                        ),
                    );
                    let _ = self.tx.send(Event::Init(Box::new(reply)));
                    self.backoff = 0;
                    self.consecutive_failures = 0;
                    conn.next_frame_at = Instant::now() + config.frame_interval();
                    self.conn = Some(conn);
                    self.set_state(
                        ConnState::Streaming,
                        format!("streaming from {}", config.addr),
                    );
                    self.stats = StatsAcc::new();
                }
                Err(err) => {
                    self.schedule_retry(format!("init payload rejected: {err}"));
                }
            },
            Err(err) => {
                self.schedule_retry(format!("init exchange failed: {err}"));
            }
        }
    }

    fn run_frame(&mut self) {
        let Some(config) = self.config else { return };
        let Some(conn) = self.conn.as_mut() else {
            return;
        };

        let started = Instant::now();
        let result = exchange_command(conn, Command::VFrame);

        match result {
            Ok(message) => {
                let rtt = started.elapsed();
                if let Err(reason) = self.accept_vframe(&message.payload, message.length, rtt) {
                    self.schedule_retry(reason);
                    return;
                }
            }
            Err(err) => {
                self.schedule_retry(format!("vFrame failed: {err}"));
                return;
            }
        }

        if let Some(conn) = self.conn.as_mut() {
            // Schedule from the request start: with one outstanding request this
            // reaches the target rate when RTT < interval, and degrades to the
            // RTT-bound rate when the device is slower. Never burst to catch up.
            conn.next_frame_at = started + config.frame_interval();
            if conn.next_frame_at < Instant::now() {
                conn.next_frame_at = Instant::now();
            }
        }

        if self.stats.last_emit.elapsed() >= STATS_INTERVAL {
            self.stats.last_emit = Instant::now();
            let _ = self.tx.send(Event::Stats(self.stats.snapshot()));
        }
    }

    /// Execute one queued, whitelisted command in the gap between frames.
    fn run_command(&mut self, command: Command) {
        let config = self.config;
        let slow = command.is_slow();
        let wire = command.wire();
        let Some(conn) = self.conn.as_mut() else {
            return;
        };

        if slow {
            let _ = conn.stream.set_read_timeout(Some(PHOTOCAP_READ_TIMEOUT));
        }

        let started = Instant::now();
        let result = match conn
            .stream
            .write_all(wire.as_bytes())
            .and_then(|()| conn.stream.flush())
        {
            Ok(()) if command.expects_reply() => {
                protocol::read_message(&mut conn.reader, command.expected_token()).map(Some)
            }
            Ok(()) => Ok(None),
            Err(err) => Err(ProtoError::Io(err)),
        };
        let elapsed = started.elapsed();

        if slow && let Some(read_timeout) = config.map(|c| c.read_timeout) {
            let _ = conn.stream.set_read_timeout(Some(read_timeout));
        }

        match result {
            Ok(Some(message)) => self.handle_reply(command, &message, elapsed),
            Ok(None) => self.log(
                Level::Info,
                format!(
                    "{} sent (no reply expected; effect appears in the next frame)",
                    command.describe()
                ),
            ),
            Err(err) => self.schedule_retry(format!("{} failed: {err}", command.describe())),
        }

        // Re-anchor pacing after an out-of-band exchange.
        let interval = config
            .map(|c| c.frame_interval())
            .unwrap_or(Duration::from_millis(50));
        if let Some(conn) = self.conn.as_mut() {
            conn.next_frame_at = Instant::now() + interval;
        }
    }

    fn handle_reply(&mut self, command: Command, message: &protocol::Message, elapsed: Duration) {
        match command {
            Command::Init => {
                if message.length != INIT_MESSAGE_LEN {
                    self.schedule_retry(format!(
                        "init reply is {} B, expected {INIT_MESSAGE_LEN}",
                        message.length
                    ));
                    return;
                }
                match InitReply::parse(&message.payload) {
                    Ok(reply) => {
                        self.log(
                            Level::Info,
                            format!(
                                "init refreshed: unit {}, ε {:.2}, {} %RH ({} ms)",
                                reply.unit_label(),
                                reply.emissivity,
                                reply.humidity_pct,
                                elapsed.as_millis()
                            ),
                        );
                        let _ = self.tx.send(Event::Init(Box::new(reply)));
                    }
                    Err(err) => self.schedule_retry(format!("init payload rejected: {err}")),
                }
            }
            Command::VFrame => {
                if let Err(reason) = self.accept_vframe(&message.payload, message.length, elapsed) {
                    self.schedule_retry(reason);
                }
            }
            Command::PhotoCap => match Capture::parse(&message.payload) {
                Ok(capture) => {
                    let stats = capture.stats();
                    self.log(
                        Level::Info,
                        format!(
                            "PhotoCap {} ({} ms): shot {} B, temp plane 192x256 \
                             {:.2}..{:.2} °C (mean {:.2}), vis {} B, y16 {} B, tail 8 B",
                            message.summary(),
                            elapsed.as_millis(),
                            capture.shot_jpeg.len(),
                            stats.min,
                            stats.max,
                            stats.mean,
                            capture.vis_jpeg.len(),
                            capture.y16.len()
                        ),
                    );
                    let _ = self.tx.send(Event::Capture(Box::new(capture)));
                }
                Err(err) => self.log(Level::Error, format!("PhotoCap payload rejected: {err}")),
            },
            Command::ColorChange(_) | Command::MeasureUnit(_) => {
                debug_assert!(!command.expects_reply());
            }
        }
    }

    /// Validate and publish one `vFrame` payload. Returns a failure reason.
    fn accept_vframe(&mut self, payload: &[u8], length: u32, rtt: Duration) -> Result<(), String> {
        // Both the base payload and the extended (live-plane) payload are valid;
        // `parse_vframe` rejects anything else.
        let parsed = protocol::parse_vframe(payload)
            .map_err(|err| format!("vFrame payload rejected: {err}"))?;

        if parsed.plane.is_some() && !self.saw_live_plane {
            self.saw_live_plane = true;
            self.log(
                Level::Info,
                "device sent an extended vFrame: live 192x256 f32 temperature plane present"
                    .to_owned(),
            );
        }

        self.seq = self.seq.wrapping_add(1);
        let frame = Arc::new(LiveFrame::decode(
            parsed.yuv,
            parsed.key_c,
            parsed.plane,
            self.seq,
            rtt,
        ));
        self.latest.store(Arc::new(Some(frame)));

        self.stats.record(rtt, length);
        self.consecutive_failures = 0;
        Ok(())
    }

    // -- teardown -----------------------------------------------------------

    fn schedule_retry(&mut self, reason: String) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.log(Level::Error, reason.clone());
        self.disconnect();

        let delay = Duration::from_millis(BACKOFF_MS[self.backoff.min(BACKOFF_MS.len() - 1)]);
        self.backoff = (self.backoff + 1).min(BACKOFF_MS.len() - 1);
        self.next_attempt = Instant::now() + delay;
        self.zero_stats();

        let failures = if self.consecutive_failures > 1 {
            format!(" [{} consecutive failures]", self.consecutive_failures)
        } else {
            String::new()
        };
        self.set_state(
            ConnState::Reconnecting,
            format!("{reason}{failures} — retry in {} ms", delay.as_millis()),
        );
    }

    /// Do not keep showing the last achieved rate while disconnected.
    fn zero_stats(&mut self) {
        self.stats.fps = 0.0;
        self.stats.rtt_ms = 0.0;
        let _ = self.tx.send(Event::Stats(self.stats.snapshot()));
    }

    fn disconnect(&mut self) {
        self.conn = None;
        if let Ok(mut slot) = self.socket_slot.lock()
            && let Some(stream) = slot.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn set_state(&mut self, state: ConnState, detail: impl Into<String>) {
        let detail = detail.into();
        if self.state == state && self.detail == detail {
            return;
        }
        self.state = state;
        self.detail = detail.clone();
        let _ = self.tx.send(Event::State { state, detail });
    }

    fn log(&self, level: Level, text: String) {
        let _ = self.tx.send(Event::Log(LogLine { level, text }));
    }
}

fn connect_socket(config: &StreamConfig) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&config.addr, config.connect_timeout)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(config.read_timeout))?;
    stream.set_write_timeout(Some(config.read_timeout))?;

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    // Keepalive is defence in depth; failure to set it is not fatal.
    let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);

    Ok(stream)
}

/// Send one whitelisted command and, when the command has a reply, read exactly
/// one framed message.
fn exchange_command(conn: &mut Conn, command: Command) -> Result<protocol::Message, ProtoError> {
    conn.stream.write_all(command.wire().as_bytes())?;
    conn.stream.flush()?;
    protocol::read_message(&mut conn.reader, command.expected_token())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::protocol::{Palette, TOKEN_INIT};
    use std::io::Read as _;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn backoff_is_capped() {
        assert_eq!(BACKOFF_MS[0], 250);
        assert_eq!(BACKOFF_MS[3], 2000);
    }

    #[test]
    fn frame_interval_tracks_target_fps() {
        let cfg = StreamConfig {
            addr: "127.0.0.1:9527".parse().unwrap(),
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(2),
            target_fps: 20,
        };
        assert_eq!(cfg.frame_interval(), Duration::from_millis(50));
    }

    #[test]
    fn states_are_labelled() {
        assert_eq!(ConnState::Streaming.label(), "streaming");
        assert!(ConnState::Reconnecting.is_active());
        assert!(!ConnState::Stopped.is_active());
    }

    /// Minimal stand-in for the camera: answers init/vFrame/PhotoCap with
    /// fixtures, accepts no-reply parameterised commands and hangs up after
    /// `frames_before_drop` frames on each connection.
    struct FakeCamera {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeCamera {
        fn spawn(frames_before_drop: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake camera");
            let addr = listener.local_addr().expect("local addr");
            let stop = Arc::new(AtomicBool::new(false));
            let handle = {
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let Ok((mut socket, _)) = listener.accept() else {
                            return;
                        };
                        let _ = socket.set_nodelay(true);
                        let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
                        if serve_connection(&mut socket, &stop, frames_before_drop).is_none() {
                            return;
                        }
                    }
                })
            };
            Self {
                addr,
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeCamera {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake a blocked accept so the server thread can observe the flag.
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Request {
        Init,
        VFrame,
        PhotoCap,
        NoReply,
    }

    /// Consume one complete request from `buffer`, if it is complete.
    fn take_request(buffer: &mut Vec<u8>) -> Option<Request> {
        const REPLY: [(&[u8], Request); 3] = [
            (b"UTi_cmdsys:init", Request::Init),
            (b"UTi_cmdsys:vFrame", Request::VFrame),
            (b"UTi_cmdsys:PhotoCap", Request::PhotoCap),
        ];
        for (prefix, kind) in REPLY {
            if buffer.starts_with(prefix) {
                buffer.drain(..prefix.len());
                return Some(kind);
            }
        }

        for prefix in [
            b"UTi_cmdsys:colorChange+".as_slice(),
            b"UTi_cmdsys:measUnit+".as_slice(),
        ] {
            if !buffer.starts_with(prefix) {
                continue;
            }
            let rest = &buffer[prefix.len()..];
            let first = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if first == 0 {
                return None;
            }
            let mut end = prefix.len() + first;
            if rest.get(first) == Some(&b'+') {
                let second = rest[first + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if second == 0 {
                    return None;
                }
                end += 1 + second;
            }
            buffer.drain(..end);
            return Some(Request::NoReply);
        }

        None
    }

    /// Returns `None` when the server should stop entirely.
    fn serve_connection(
        socket: &mut TcpStream,
        stop: &AtomicBool,
        frames_before_drop: usize,
    ) -> Option<()> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 256];
        let mut frames = 0usize;
        loop {
            if stop.load(Ordering::SeqCst) {
                return None;
            }
            let read = match socket.read(&mut chunk) {
                Ok(0) => return Some(()),
                Ok(n) => n,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => return Some(()),
            };
            buffer.extend_from_slice(&chunk[..read]);

            while let Some(request) = take_request(&mut buffer) {
                let reply = match request {
                    Request::Init => fixtures::init_message(),
                    Request::PhotoCap => fixtures::photocap_message(),
                    Request::NoReply => continue,
                    Request::VFrame => {
                        frames += 1;
                        if frames > frames_before_drop {
                            // Simulate a camera power cycle / AP drop.
                            return Some(());
                        }
                        // Real firmware appends a live plane to every 11th frame.
                        if frames.is_multiple_of(11) {
                            fixtures::vframe_extended_message()
                        } else {
                            fixtures::vframe_message()
                        }
                    }
                };
                if socket.write_all(&reply).is_err() {
                    return Some(());
                }
            }
        }
    }

    fn start_worker(camera: &FakeCamera, fps: u32) -> Worker {
        let worker = Worker::spawn().expect("spawn worker");
        worker.start(StreamConfig {
            addr: camera.addr,
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_millis(500),
            target_fps: fps,
        });
        worker
    }

    #[test]
    fn streams_reinitialises_and_recovers_from_a_dropped_link() {
        let camera = FakeCamera::spawn(3);
        let worker = start_worker(&camera, 60);

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut inits = 0usize;
        let mut reconnect_events = 0usize;
        let mut streaming_events = 0usize;
        let mut best_seq = 0u64;

        while Instant::now() < deadline {
            for event in worker.try_events() {
                match event {
                    Event::Init(_) => inits += 1,
                    Event::State { state, .. } => match state {
                        ConnState::Streaming => streaming_events += 1,
                        ConnState::Reconnecting => reconnect_events += 1,
                        _ => {}
                    },
                    _ => {}
                }
            }
            if let Some(frame) = worker.latest_frame() {
                best_seq = best_seq.max(frame.seq);
            }
            if inits >= 2 && best_seq >= 4 && reconnect_events >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        assert!(
            inits >= 2,
            "init must be re-run after a reconnect, got {inits}"
        );
        assert!(
            streaming_events >= 2,
            "expected streaming state after each init, got {streaming_events}"
        );
        assert!(
            reconnect_events >= 1,
            "expected at least one reconnect after the dropped link"
        );
        assert!(
            best_seq >= 4,
            "frames must resume after reconnecting, got seq {best_seq}"
        );

        worker.stop();
    }

    #[test]
    fn no_reply_command_does_not_stall_the_frame_loop() {
        let camera = FakeCamera::spawn(usize::MAX);
        let worker = start_worker(&camera, 60);

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut before = 0u64;
        while Instant::now() < deadline {
            if let Some(frame) = worker.latest_frame() {
                before = frame.seq;
                if before >= 2 {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(before >= 2, "stream did not start, seq {before}");

        worker.run(Command::ColorChange(Palette::P3));

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut after = before;
        let mut sent_log = false;
        while Instant::now() < deadline {
            for event in worker.try_events() {
                if let Event::Log(line) = event
                    && line.text.contains("no reply expected")
                {
                    sent_log = true;
                }
            }
            if let Some(frame) = worker.latest_frame() {
                after = frame.seq;
            }
            if sent_log && after >= before + 3 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        assert!(sent_log, "worker should report the no-reply command");
        assert!(
            after >= before + 3,
            "frames must keep flowing after a no-reply command (seq {before} -> {after})"
        );

        worker.stop();
    }

    #[test]
    fn extended_frames_keep_the_stream_alive() {
        let camera = FakeCamera::spawn(usize::MAX);
        let worker = start_worker(&camera, 20);

        let deadline = Instant::now() + Duration::from_secs(6);
        let mut best_seq = 0u64;
        let mut saw_plane = false;
        let mut saw_reconnect = false;
        let mut inits = 0usize;

        while Instant::now() < deadline {
            for event in worker.try_events() {
                match event {
                    Event::State {
                        state: ConnState::Reconnecting,
                        ..
                    } => saw_reconnect = true,
                    Event::Init(_) => inits += 1,
                    _ => {}
                }
            }
            if let Some(frame) = worker.latest_frame() {
                best_seq = best_seq.max(frame.seq);
                saw_plane |= frame.plane_c.is_some();
            }
            if best_seq >= 25 && saw_plane {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        assert!(best_seq >= 25, "stream stalled at seq {best_seq}");
        assert!(saw_plane, "no extended frame with a live plane");
        assert!(!saw_reconnect, "extended frames must not force reconnects");
        assert_eq!(inits, 1, "init should run once, ran {inits} times");

        worker.stop();
    }

    #[test]
    fn photo_cap_command_surfaces_a_capture() {
        let camera = FakeCamera::spawn(usize::MAX);
        let worker = start_worker(&camera, 60);

        // Wait until streaming before asking for a capture.
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline && worker.latest_frame().is_none() {
            thread::sleep(Duration::from_millis(20));
        }
        worker.run(Command::PhotoCap);

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut capture = None;
        while Instant::now() < deadline && capture.is_none() {
            for event in worker.try_events() {
                if let Event::Capture(capture_event) = event {
                    capture = Some(capture_event);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }

        let capture = capture.expect("PhotoCap event");
        assert_eq!(
            capture.temperature_c.len(),
            crate::capture::TEMP_PLANE_SAMPLES
        );
        assert!(capture.stats().max > capture.stats().min);
        assert!(capture.shot_jpeg.starts_with(&[0xFF, 0xD8]));

        worker.stop();
    }

    #[test]
    fn rejects_wrong_init_message_length() {
        // A well-framed init reply whose payload is one byte short: framing is
        // self-consistent but the documented length (160) does not match.
        let mut payload = fixtures::init_payload();
        payload.pop();
        let message = protocol::encode_message(TOKEN_INIT, &payload);
        let mut cursor = io::Cursor::new(message);
        let parsed = protocol::read_message(&mut cursor, Some(TOKEN_INIT)).expect("framing ok");
        assert_ne!(parsed.length, INIT_MESSAGE_LEN);
        assert!(InitReply::parse(&parsed.payload).is_err());
    }
}
