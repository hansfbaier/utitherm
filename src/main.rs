//! `uti260b-viewer` — independent Rust + egui client for the UNI-T UTi260B+
//! live-view protocol (TCP 9527).
//!
//! See `UTi260B-plus-protocol-spec.md`. The streamed image is palette-mapped YUV
//! with three unlabelled key temperatures. Per-pixel radiometry comes from the
//! `PhotoCap` path (192x256 f32 plane); commands are a typed whitelist because
//! malformed arguments kill the device daemon.

mod app;
mod capture;
mod config;
mod fixtures;
mod frame;
mod net;
mod protocol;
mod replay;
mod yuv;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::ExitCode;

use config::{CameraAddr, Config};

const USAGE: &str = "\
uti260b-viewer — UTi260B+ thermal imager live viewer (TCP 9527)

USAGE:
    uti260b-viewer [OPTIONS]

OPTIONS:
    --ip <IPV4>            Camera address override (default: [camera] ip or auto)
    --port <PORT>          Live-view TCP port (default 9527)
    --fps <N>              Target frame rate, 1..=60 (default 20)
    --config <PATH>        Config file (default: ./uti260b.toml, then
                           ~/.config/uti260b/config.toml)
    --out-dir <DIR>        Where PNG/YUV exports go (default ./captures)
    --replay <PATH>        Replay framed .bin fixtures or raw .yuv files
                           (single file or directory) instead of connecting
    --make-fixtures <DIR>  Write synthetic fixtures (init.bin, vframe.bin,
                           live_clean.yuv, photocap.bin) and exit
    --verify-capture <PATH>
                           Offline check of a stored PhotoCap reply: parse the
                           sections and assert the documented luma correlation
                           (>= 0.8) against the OSD screenshot
    -h, --help             Print this help
    -V, --version          Print version

KEYS:
    Space                  Connect / disconnect while not typing

CONFIG (TOML subset):
    [camera]
    ip = \"auto\"             # \"auto\" = default gateway of the camera AP
    live_port = 9527
    connect_timeout_ms = 2000
    read_timeout_ms = 2000
    target_fps = 20
    out_dir = \"captures\"

NOTES:
    A device account, credential or fixed camera IP is not needed or stored.
    'auto' reads the host default gateway: while joined to the camera's AP the
    camera *is* the gateway. If discovery is unavailable, pass --ip.

    The command channel is a typed whitelist (no free text): the device daemon
    exits on a malformed command and needs a power cycle. PhotoCap writes a file
    to the camera SD card and takes about 3 seconds.
";

#[derive(Default)]
struct Cli {
    ip: Option<Ipv4Addr>,
    port: Option<u16>,
    fps: Option<u32>,
    config: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    replay: Option<PathBuf>,
    make_fixtures: Option<PathBuf>,
    verify_capture: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(cli) = parse_args()? else {
        return Ok(());
    };

    if let Some(dir) = &cli.make_fixtures {
        let written = fixtures::write_all(dir)
            .map_err(|e| format!("cannot write fixtures to {}: {e}", dir.display()))?;
        for path in written {
            println!("{}", path.display());
        }
        return Ok(());
    }

    if let Some(path) = &cli.verify_capture {
        let report = capture::verify_capture_file(path)?;
        println!(
            "capture: {} B shot JPEG, {} B vis JPEG, {} temperature samples (192x256), {} B y16",
            report.shot_jpeg_bytes,
            report.vis_jpeg_bytes,
            report.temperature_samples,
            report.y16_bytes
        );
        println!(
            "temperature: {:.2} .. {:.2} °C, mean {:.2}, sigma {:.2}",
            report.stats.min, report.stats.max, report.stats.mean, report.stats.std_dev
        );
        println!(
            "luma correlation at display/1.25, no rotation or flip: {:.3}",
            report.correlation
        );
        if report.correlation < 0.8 {
            return Err(format!(
                "correlation {:.3} is below the documented 0.8 threshold; \
                 check the 192-sample stride and the display mapping",
                report.correlation
            ));
        }
        println!("ok: correlation >= 0.8");
        return Ok(());
    }

    let (mut config, mut notes) = Config::load(cli.config.as_deref())?;
    if let Some(ip) = cli.ip {
        config.camera = CameraAddr::Manual(ip);
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
    if let Some(dir) = cli.out_dir.clone() {
        config.out_dir = dir;
    }

    let mode = match &cli.replay {
        Some(path) => {
            if !path.exists() {
                return Err(format!("replay path {} does not exist", path.display()));
            }
            app::Mode::Replay { path: path.clone() }
        }
        None => app::Mode::Live,
    };

    if matches!(mode, app::Mode::Live) {
        // Best-effort early check so the log explains a missing camera before the
        // user starts clicking; the UI can retry once the AP is joined.
        match config.resolve_ip() {
            Ok(ip) => notes.push(format!("camera address resolved to {ip}")),
            Err(err) => notes.push(err),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("UTi260B+ live view")
            .with_inner_size([1180.0, 860.0])
            .with_min_inner_size([820.0, 600.0]),
        ..Default::default()
    };

    let config_for_app = config;
    eframe::run_native(
        "UTi260B+ live view",
        options,
        Box::new(move |cc| {
            let app = app::CameraApp::new(cc, config_for_app, mode, notes)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| format!("eframe: {e}"))
}

fn parse_args() -> Result<Option<Cli>, String> {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("uti260b-viewer {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--ip" => {
                let value = next_value(&mut args, "--ip")?;
                cli.ip = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--ip: {value:?} is not an IPv4 address"))?,
                );
            }
            "--port" => {
                let value = next_value(&mut args, "--port")?;
                cli.port = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--port: {value:?} is not a TCP port"))?,
                );
            }
            "--fps" => {
                let value = next_value(&mut args, "--fps")?;
                cli.fps = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--fps: {value:?} is not an integer"))?,
                );
            }
            "--config" => cli.config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            "--out-dir" => cli.out_dir = Some(PathBuf::from(next_value(&mut args, "--out-dir")?)),
            "--replay" => cli.replay = Some(PathBuf::from(next_value(&mut args, "--replay")?)),
            "--make-fixtures" => {
                cli.make_fixtures = Some(PathBuf::from(next_value(&mut args, "--make-fixtures")?));
            }
            "--verify-capture" => {
                cli.verify_capture =
                    Some(PathBuf::from(next_value(&mut args, "--verify-capture")?));
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }

    Ok(Some(cli))
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}
