//! Configuration (§8.3) and camera address discovery (§1).
//!
//! The renderer never hardcodes an IP: `ip = "auto"` resolves the default
//! gateway of the host's route table (the camera is the DHCP server on its own
//! AP), with a manual override via config file or `--ip`.
//!
//! Config file format is a small TOML subset — sections, `key = value`, `#`
//! comments — enough for the documented file:
//!
//! ```toml
//! [camera]
//! ip = "auto"
//! live_port = 9527
//! connect_timeout_ms = 2000
//! read_timeout_ms = 2000
//! target_fps = 20
//! ```

use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 9527;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 2000;
pub const DEFAULT_READ_TIMEOUT_MS: u64 = 2000;
pub const DEFAULT_TARGET_FPS: u32 = 20;
pub const DEFAULT_CONFIG_NAME: &str = "uti260b.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraAddr {
    Auto,
    Manual(Ipv4Addr),
}

impl fmt::Display for CameraAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Manual(ip) => write!(f, "{ip}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub camera: CameraAddr,
    pub live_port: u16,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub target_fps: u32,
    pub out_dir: PathBuf,
    /// Config file actually loaded, if any.
    pub source: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            camera: CameraAddr::Auto,
            live_port: DEFAULT_PORT,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            read_timeout_ms: DEFAULT_READ_TIMEOUT_MS,
            target_fps: DEFAULT_TARGET_FPS,
            out_dir: PathBuf::from("captures"),
            source: None,
        }
    }
}

impl Config {
    /// Load `explicit` when given (must exist), otherwise the first existing
    /// candidate of the local and user config paths.
    ///
    /// Returns the config plus non-fatal warnings (unknown keys, bad values).
    pub fn load(explicit: Option<&Path>) -> Result<(Self, Vec<String>), String> {
        let mut warnings = Vec::new();
        let candidates: Vec<PathBuf> = match explicit {
            Some(path) => {
                if !path.exists() {
                    return Err(format!("config file {} does not exist", path.display()));
                }
                vec![path.to_path_buf()]
            }
            None => default_config_paths(),
        };

        let Some(path) = candidates.into_iter().find(|p| p.exists()) else {
            return Ok((Self::default(), warnings));
        };

        let text = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut config = Self {
            source: Some(path.clone()),
            ..Self::default()
        };
        let mut section = String::new();

        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!(
                    "{}:{}: ignoring line without '=': {raw}",
                    path.display(),
                    lineno + 1
                ));
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"').trim_matches('\'');

            match (section.as_str(), key.as_str()) {
                ("camera", "ip") | ("", "ip") => match value {
                    "auto" | "" => config.camera = CameraAddr::Auto,
                    other => match other.parse::<Ipv4Addr>() {
                        Ok(ip) => config.camera = CameraAddr::Manual(ip),
                        Err(_) => warnings.push(format!(
                            "{}:{}: invalid ip {other:?} (use an IPv4 address or \"auto\")",
                            path.display(),
                            lineno + 1
                        )),
                    },
                },
                ("camera", "live_port") => match value.parse::<u16>() {
                    Ok(port) if port > 0 => config.live_port = port,
                    _ => warnings.push(format!(
                        "{}:{}: invalid live_port {value:?}",
                        path.display(),
                        lineno + 1
                    )),
                },
                ("camera", "connect_timeout_ms") => match value.parse::<u64>() {
                    Ok(ms) if ms > 0 => config.connect_timeout_ms = ms,
                    _ => warnings.push(format!(
                        "{}:{}: invalid connect_timeout_ms {value:?}",
                        path.display(),
                        lineno + 1
                    )),
                },
                ("camera", "read_timeout_ms") => match value.parse::<u64>() {
                    Ok(ms) if ms > 0 => config.read_timeout_ms = ms,
                    _ => warnings.push(format!(
                        "{}:{}: invalid read_timeout_ms {value:?}",
                        path.display(),
                        lineno + 1
                    )),
                },
                ("camera", "target_fps") => match value.parse::<u32>() {
                    Ok(fps) if (1..=60).contains(&fps) => config.target_fps = fps,
                    _ => warnings.push(format!(
                        "{}:{}: invalid target_fps {value:?} (1..=60)",
                        path.display(),
                        lineno + 1
                    )),
                },
                ("camera", "out_dir") => config.out_dir = PathBuf::from(value),
                ("", _) | ("camera", _) => warnings.push(format!(
                    "{}:{}: unknown key {key:?} in section {section:?}",
                    path.display(),
                    lineno + 1
                )),
                _ => {
                    // Unknown sections are reserved for future use; ignore quietly.
                }
            }
        }

        Ok((config, warnings))
    }

    /// Resolve the address to connect to: manual override or gateway lookup.
    pub fn resolve_ip(&self) -> Result<Ipv4Addr, String> {
        match self.camera {
            CameraAddr::Manual(ip) => Ok(ip),
            CameraAddr::Auto => default_gateway_ipv4(),
        }
    }

    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn read_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.read_timeout_ms)
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(DEFAULT_CONFIG_NAME)];
    if let Some(home) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        paths.push(home.join("uti260b").join("config.toml"));
    }
    paths
}

/// Default gateway of the host, preferring a wireless interface.
///
/// The camera is the DHCP server of its own access point, so its address is the
/// client's default gateway while the camera AP is the active network (§1).
pub fn default_gateway_ipv4() -> Result<Ipv4Addr, String> {
    gateway_impl().map_err(|e| {
        format!("cannot determine camera IP automatically: {e}. Set [camera] ip = \"192.168.x.x\" or pass --ip.")
    })
}

#[cfg(target_os = "linux")]
fn gateway_impl() -> Result<Ipv4Addr, String> {
    let text = fs::read_to_string("/proc/net/route")
        .map_err(|e| format!("reading /proc/net/route: {e}"))?;

    let mut best: Option<(bool, Ipv4Addr)> = None;
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(iface), Some(dest), Some(gateway), Some(flags)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if dest != "00000000" {
            continue;
        }
        let Ok(flags) = u32::from_str_radix(flags, 16) else {
            continue;
        };
        const RTF_GATEWAY: u32 = 0x2;
        if flags & RTF_GATEWAY == 0 {
            continue;
        }
        let Ok(raw) = u32::from_str_radix(gateway, 16) else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        // /proc/net/route prints the gateway as little-endian hex.
        let ip = Ipv4Addr::from(raw.to_le_bytes());
        let wireless = iface.starts_with("wl") || iface.starts_with("wlan");
        let better = match best {
            None => true,
            Some((was_wireless, _)) => wireless && !was_wireless,
        };
        if better {
            best = Some((wireless, ip));
        }
    }

    best.map(|(_, ip)| ip)
        .ok_or_else(|| "no default gateway with the RTF_GATEWAY flag in /proc/net/route".into())
}

#[cfg(target_os = "macos")]
fn gateway_impl() -> Result<Ipv4Addr, String> {
    let output = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
        .map_err(|e| format!("running route(8): {e}"))?;
    if !output.status.success() {
        return Err("route -n get default failed".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "gateway" {
                return value
                    .trim()
                    .parse::<Ipv4Addr>()
                    .map_err(|e| format!("parsing gateway {value:?}: {e}"));
            }
        }
    }
    Err("no 'gateway:' line in route output".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn gateway_impl() -> Result<Ipv4Addr, String> {
    Err("automatic gateway discovery is only implemented on Linux and macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn defaults_match_spec() {
        let cfg = Config::default();
        assert_eq!(cfg.camera, CameraAddr::Auto);
        assert_eq!(cfg.live_port, 9527);
        assert_eq!(cfg.connect_timeout_ms, 2000);
        assert_eq!(cfg.read_timeout_ms, 2000);
        assert_eq!(cfg.target_fps, 20);
    }

    #[test]
    fn parses_documented_config() {
        let dir = std::env::temp_dir().join(format!(
            "uti260b-config-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "[camera]\nip = \"192.168.27.10\"\nlive_port = 9527\nconnect_timeout_ms = 1500\nread_timeout_ms = 2500\ntarget_fps = 15\n"
        )
        .unwrap();
        drop(f);

        let (cfg, warnings) = Config::load(Some(&path)).expect("load config");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            cfg.camera,
            CameraAddr::Manual("192.168.27.10".parse().unwrap())
        );
        assert_eq!(cfg.connect_timeout_ms, 1500);
        assert_eq!(cfg.read_timeout_ms, 2500);
        assert_eq!(cfg.target_fps, 15);
        assert_eq!(cfg.resolve_ip().unwrap().to_string(), "192.168.27.10");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_values_without_failing() {
        let dir = std::env::temp_dir().join(format!(
            "uti260b-config-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[camera]\nip = \"999.1.1.1\"\ntarget_fps = 0\nbogus = 1\n",
        )
        .unwrap();

        let (cfg, warnings) = Config::load(Some(&path)).expect("load config");
        assert_eq!(cfg.camera, CameraAddr::Auto);
        assert_eq!(cfg.target_fps, DEFAULT_TARGET_FPS);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let err = Config::load(Some(Path::new("/nonexistent/uti260b.toml"))).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }
}
