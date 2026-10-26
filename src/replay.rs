//! Fixture replay source (§13.8).
//!
//! Drives the *same* UI path as the network worker: framed `.bin` messages are
//! re-parsed through the production framing/validation code, raw `.yuv` files
//! are treated as bare pixel blocks (no key temperatures).
//!
//! Accepted files: full framed messages (`init.bin`, `vframe.bin`, concatenated
//! messages are fine), raw 115 200-byte YUV 420 blocks (`live_clean.yuv`), or a
//! directory containing any mix of those.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::capture::Capture;
use crate::frame::LiveFrame;
use crate::model::{self, PixelFormat};
use crate::net::{ConnState, Event, Level, LogLine, NetStats};
use crate::protocol::{self, InitReply, TOKEN_INIT, TOKEN_PHOTOCAP, TOKEN_VFRAME, YUV_LEN};

/// How often replay pushes stats so the UI looks like a live source.
const STATS_INTERVAL: Duration = Duration::from_millis(500);
const CONTROL_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
struct FrameFixture {
    yuv: Vec<u8>,
    format: PixelFormat,
    display_w: usize,
    display_h: usize,
    raw_w: usize,
    raw_h: usize,
    keys: [f32; 3],
    plane: Option<Vec<u8>>,
    source: String,
}

#[derive(Default, Debug)]
struct FixtureSet {
    init: Option<InitReply>,
    capture: Option<Capture>,
    frames: Vec<FrameFixture>,
    files: usize,
    skipped: Vec<String>,
}

pub struct ReplayWorker {
    tx: Sender<ReplayMsg>,
    events: Receiver<Event>,
    latest: Arc<ArcSwap<Option<Arc<LiveFrame>>>>,
    join: Option<JoinHandle<()>>,
}

enum ReplayMsg {
    SetRunning(bool),
    SetFps(u32),
    Shutdown,
}

impl ReplayWorker {
    /// Load `path` and start playing it back at `fps`.
    pub fn spawn(path: &Path, fps: u32) -> Result<Self, String> {
        let fixtures = load(path)?;
        if fixtures.frames.is_empty() {
            return Err(format!(
                "no replayable frames in {} (need framed vFrame messages or {YUV_LEN}-byte .yuv files)",
                path.display()
            ));
        }

        let (tx, rx) = unbounded();
        let (event_tx, event_rx) = unbounded::<Event>();
        let latest = Arc::new(ArcSwap::from_pointee(None::<Arc<LiveFrame>>));

        let join = {
            let latest = Arc::clone(&latest);
            thread::Builder::new()
                .name("utitherm-replay".into())
                .spawn(move || run(fixtures, rx, event_tx, latest, fps))
                .map_err(|e| format!("cannot start replay thread: {e}"))?
        };

        Ok(Self {
            tx,
            events: event_rx,
            latest,
            join: Some(join),
        })
    }

    pub fn latest_frame(&self) -> Option<Arc<LiveFrame>> {
        self.latest.load().as_ref().as_ref().map(Arc::clone)
    }

    pub fn try_events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    pub fn set_running(&self, running: bool) {
        let _ = self.tx.send(ReplayMsg::SetRunning(running));
    }

    pub fn set_fps(&self, fps: u32) {
        let _ = self.tx.send(ReplayMsg::SetFps(fps));
    }
}

impl Drop for ReplayWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(ReplayMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(
    fixtures: FixtureSet,
    rx: Receiver<ReplayMsg>,
    tx: Sender<Event>,
    latest: Arc<ArcSwap<Option<Arc<LiveFrame>>>>,
    initial_fps: u32,
) {
    let mut fps = initial_fps.clamp(1, 60);
    let mut running = true;
    let mut seq = 0u64;
    let mut index = 0usize;
    let mut frames_since_stats = 0u32;
    let mut last_stats = Instant::now();

    let _ = tx.send(Event::Log(LogLine {
        level: Level::Info,
        text: format!(
            "replay: {} frames from {} file(s)",
            fixtures.frames.len(),
            fixtures.files
        ),
    }));
    let mut sources: Vec<&str> = Vec::new();
    for frame in &fixtures.frames {
        if !sources.contains(&frame.source.as_str()) {
            sources.push(&frame.source);
        }
    }
    for source in sources.iter().take(8) {
        let _ = tx.send(Event::Log(LogLine {
            level: Level::Info,
            text: format!("replay source: {source}"),
        }));
    }
    for skipped in &fixtures.skipped {
        let _ = tx.send(Event::Log(LogLine {
            level: Level::Warn,
            text: skipped.clone(),
        }));
    }
    if let Some(init) = fixtures.init.clone() {
        let _ = tx.send(Event::Init(Box::new(init)));
    }
    if let Some(capture) = fixtures.capture.clone() {
        let _ = tx.send(Event::Capture(Box::new(capture)));
    }
    let _ = tx.send(Event::State {
        state: ConnState::Streaming,
        detail: "fixture replay".into(),
    });

    loop {
        // Control messages.
        while let Ok(msg) = rx.try_recv() {
            if !handle_control(msg, &mut running, &mut fps, &tx) {
                return;
            }
        }

        if !running {
            match rx.recv_timeout(CONTROL_POLL) {
                Ok(msg) => {
                    if !handle_control(msg, &mut running, &mut fps, &tx) {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        let fixture = &fixtures.frames[index % fixtures.frames.len()];
        index = index.wrapping_add(1);
        seq = seq.wrapping_add(1);

        let frame = Arc::new(LiveFrame::decode_with(
            &fixture.yuv,
            fixture.format,
            fixture.display_w,
            fixture.display_h,
            fixture.raw_w,
            fixture.raw_h,
            fixture.keys,
            fixture.plane.as_deref(),
            seq,
            Duration::ZERO,
        ));
        latest.store(Arc::new(Some(Arc::clone(&frame))));

        frames_since_stats = frames_since_stats.saturating_add(1);
        if last_stats.elapsed() >= STATS_INTERVAL {
            let elapsed = last_stats.elapsed().as_secs_f32();
            let _ = tx.send(Event::Stats(NetStats {
                rtt_ms: 0.0,
                fps: frames_since_stats as f32 / elapsed.max(0.001),
                frames: seq,
                last_length: (fixture.yuv.len()
                    + 12
                    + fixture.plane.as_ref().map_or(0, Vec::len)
                    + BOXED_OVERHEAD as usize) as u32,
            }));
            frames_since_stats = 0;
            last_stats = Instant::now();
        }

        // Pace the loop; also serves as the control-polling point.
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        match rx.recv_timeout(interval) {
            Ok(msg) => {
                if !handle_control(msg, &mut running, &mut fps, &tx) {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Apply one control message. Returns `false` when the worker should exit.
fn handle_control(msg: ReplayMsg, running: &mut bool, fps: &mut u32, tx: &Sender<Event>) -> bool {
    match msg {
        ReplayMsg::SetRunning(next) => {
            *running = next;
            let (state, detail) = if *running {
                (ConnState::Streaming, "fixture replay")
            } else {
                (ConnState::Stopped, "replay paused")
            };
            let _ = tx.send(Event::State {
                state,
                detail: detail.into(),
            });
            true
        }
        ReplayMsg::SetFps(next) => {
            *fps = next.clamp(1, 60);
            true
        }
        ReplayMsg::Shutdown => false,
    }
}

/// Framed vFrame overhead: 9-byte token + 4-byte length + CRLF.
const BOXED_OVERHEAD: u32 = 15;

fn load(path: &Path) -> Result<FixtureSet, String> {
    let files = collect_files(path)?;
    let mut set = FixtureSet {
        files: files.len(),
        ..FixtureSet::default()
    };

    for file in files {
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(err) => {
                set.skipped
                    .push(format!("{}: cannot read: {err}", file.display()));
                continue;
            }
        };

        // Bare pixel block: exact YUV size and not a plausible frame token.
        let looks_raw = bytes.len() == YUV_LEN && bytes.first() != Some(&b'{');
        if looks_raw || file.extension().is_some_and(|ext| ext == "yuv") {
            if bytes.len() != YUV_LEN {
                set.skipped.push(format!(
                    "{}: raw YUV file is {} B, expected {YUV_LEN}",
                    file.display(),
                    bytes.len()
                ));
                continue;
            }
            set.frames.push(FrameFixture {
                yuv: bytes,
                format: model::REFERENCE.format,
                display_w: model::REFERENCE.display_w,
                display_h: model::REFERENCE.display_h,
                raw_w: model::REFERENCE.raw_w,
                raw_h: model::REFERENCE.raw_h,
                keys: [f32::NAN; 3],
                plane: None,
                source: file.display().to_string(),
            });
            continue;
        }

        let mut cursor = Cursor::new(&bytes);
        let mut parsed_any = false;
        loop {
            if cursor.position() as usize >= bytes.len() {
                break;
            }
            match protocol::read_message(&mut cursor, None) {
                Ok(message) => {
                    parsed_any = true;
                    match message.token.as_str() {
                        TOKEN_INIT => match InitReply::parse(&message.payload) {
                            Ok(reply) => set.init = Some(reply),
                            Err(err) => set
                                .skipped
                                .push(format!("{}: init payload rejected: {err}", file.display())),
                        },
                        TOKEN_PHOTOCAP => match Capture::parse(&message.payload) {
                            Ok(capture) => set.capture = Some(capture),
                            Err(err) => set.skipped.push(format!(
                                "{}: PhotoCap payload rejected: {err}",
                                file.display()
                            )),
                        },
                        TOKEN_VFRAME => match protocol::parse_vframe(&message.payload) {
                            Ok(frame) => set.frames.push(FrameFixture {
                                yuv: frame.yuv.to_vec(),
                                format: frame.format,
                                display_w: frame.display_w,
                                display_h: frame.display_h,
                                raw_w: frame.raw_w,
                                raw_h: frame.raw_h,
                                keys: frame.key_c,
                                plane: frame.plane.map(|plane| plane.to_vec()),
                                source: file.display().to_string(),
                            }),
                            Err(err) => set.skipped.push(format!(
                                "{}: vFrame payload rejected: {err}",
                                file.display()
                            )),
                        },
                        other => set
                            .skipped
                            .push(format!("{}: ignoring token {other:?}", file.display())),
                    }
                }
                Err(err) => {
                    if !parsed_any && bytes.len() == YUV_LEN {
                        set.frames.push(FrameFixture {
                            yuv: bytes,
                            format: model::REFERENCE.format,
                            display_w: model::REFERENCE.display_w,
                            display_h: model::REFERENCE.display_h,
                            raw_w: model::REFERENCE.raw_w,
                            raw_h: model::REFERENCE.raw_h,
                            keys: [f32::NAN; 3],
                            plane: None,
                            source: file.display().to_string(),
                        });
                    } else {
                        set.skipped
                            .push(format!("{}: framing error: {err}", file.display()));
                    }
                    break;
                }
            }
        }
    }

    Ok(set)
}

fn collect_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(format!(
            "{} is neither a file nor a directory",
            path.display()
        ));
    }

    let mut files = Vec::new();
    let entries = fs::read_dir(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading {}: {e}", path.display()))?;
        let candidate = entry.path();
        if !candidate.is_file() {
            continue;
        }
        let ext = candidate
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(
            ext.as_str(),
            "yuv" | "bin" | "dat" | "msg" | "raw" | "frame"
        ) {
            files.push(candidate);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_framed_and_raw_fixtures_from_directory() {
        let dir = std::env::temp_dir().join(format!(
            "utitherm-replay-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        crate::fixtures::write_all(&dir).expect("write fixtures");

        let set = load(&dir).expect("load replay dir");
        assert_eq!(set.files, 4);
        assert!(set.init.is_some(), "init.bin should parse");
        assert_eq!(set.frames.len(), 2, "vframe.bin + live_clean.yuv");
        let framed = set
            .frames
            .iter()
            .find(|frame| frame.keys[0].is_finite())
            .expect("one framed vFrame fixture with key temperatures");
        assert_eq!(framed.keys, crate::fixtures::SAMPLE_KEY_C);
        assert!(set.skipped.is_empty(), "{:?}", set.skipped);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_wrong_raw_yuv_size() {
        let dir = std::env::temp_dir().join(format!(
            "utitherm-replay-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.yuv"), vec![0u8; 10]).unwrap();

        let set = load(&dir).expect("load dir");
        assert!(set.frames.is_empty());
        assert_eq!(set.skipped.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_is_an_error() {
        let err = load(Path::new("/nonexistent/fixtures")).unwrap_err();
        assert!(err.contains("neither a file nor a directory"), "{err}");
    }

    #[test]
    fn worker_publishes_frames_and_the_init_event() {
        let dir = std::env::temp_dir().join(format!(
            "utitherm-replay-worker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        crate::fixtures::write_all(&dir).expect("write fixtures");

        let worker = ReplayWorker::spawn(&dir, 60).expect("spawn replay worker");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_init = false;
        let mut frame = None;

        while Instant::now() < deadline && (frame.is_none() || !saw_init) {
            for event in worker.try_events() {
                if matches!(event, crate::net::Event::Init(_)) {
                    saw_init = true;
                }
            }
            frame = worker.latest_frame();
            thread::sleep(Duration::from_millis(10));
        }

        assert!(saw_init, "init.bin must surface as an Init event");
        let frame = frame.expect("replay must publish a decoded frame");
        assert!(frame.seq > 0);
        assert_eq!(
            frame.rgba.len(),
            crate::protocol::FRAME_W * crate::protocol::FRAME_H * 4
        );

        drop(worker);
        let _ = fs::remove_dir_all(&dir);
    }
}
