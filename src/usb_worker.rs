//! USB import worker (§11.6).
//!
//! Read-only scan of a mounted camera volume. Parsing, hashing, copying and
//! thumbnail generation run on this thread; the UI only receives events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::net::Level;
use crate::usb::{self, IndexEntry, UsbIndex};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STABILISE_DELAY: Duration = Duration::from_millis(300);
const RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct ImportedUsb {
    pub entry: IndexEntry,
    pub plane: Arc<Vec<f32>>,
    pub thumbnail_rgba: Option<Vec<u8>>,
    pub duplicate: bool,
}

#[derive(Debug)]
pub enum UsbEvent {
    /// Human-readable volume list; empty when nothing is mounted.
    Volumes(Vec<String>),
    Status(String),
    Imported(Box<ImportedUsb>),
    Error(String),
    Log {
        level: Level,
        text: String,
    },
}

enum UsbMsg {
    Rescan,
    SetPaused(bool),
    Shutdown,
}

pub struct UsbWorker {
    tx: Sender<UsbMsg>,
    events: Receiver<UsbEvent>,
    cache: PathBuf,
    join: Option<JoinHandle<()>>,
}

impl UsbWorker {
    pub fn spawn(
        cache: PathBuf,
        explicit: Option<PathBuf>,
        marker: String,
    ) -> std::io::Result<Self> {
        let (tx, rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let cache_for_thread = cache.clone();
        let join = thread::Builder::new()
            .name("utitherm-usb".into())
            .spawn(move || run(rx, event_tx, cache_for_thread, explicit, marker))?;
        Ok(Self {
            tx,
            events: event_rx,
            cache,
            join: Some(join),
        })
    }

    pub fn rescan(&self) {
        let _ = self.tx.send(UsbMsg::Rescan);
    }

    /// Pause scanning while a live network session is active (§11.6 UI rule).
    pub fn set_paused(&self, paused: bool) {
        let _ = self.tx.send(UsbMsg::SetPaused(paused));
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    pub fn try_events(&self) -> Vec<UsbEvent> {
        self.events.try_iter().collect()
    }
}

impl Drop for UsbWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(UsbMsg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(
    rx: Receiver<UsbMsg>,
    tx: Sender<UsbEvent>,
    cache: PathBuf,
    explicit: Option<PathBuf>,
    marker: String,
) {
    let mut index = usb::load_index(&cache);
    usb::refresh_index(&cache, &mut index);
    emit_existing(&tx, &cache, &index);

    let mut seen: HashMap<PathBuf, (u64, u128)> = HashMap::new();
    let mut last_volumes: Option<Vec<String>> = None;
    let mut paused = false;
    let _ = tx.send(UsbEvent::Status(
        "scanning for a UTi260B+ volume".to_owned(),
    ));

    loop {
        // Control messages are handled between scans; the worker may be blocked
        // reading a file for a few milliseconds, never on the UI.
        if let Ok(msg) = rx.try_recv() {
            match msg {
                UsbMsg::Shutdown => return,
                UsbMsg::Rescan => {
                    seen.clear();
                    let _ = tx.send(UsbEvent::Status("rescan requested".to_owned()));
                }
                UsbMsg::SetPaused(next) => {
                    paused = next;
                    let status = if paused {
                        "USB import paused while the live session is active"
                    } else {
                        "watching for a camera volume"
                    };
                    let _ = tx.send(UsbEvent::Status(status.to_owned()));
                }
            }
        }

        if paused {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(UsbMsg::Shutdown) => return,
                Ok(UsbMsg::SetPaused(next)) => paused = next,
                Ok(UsbMsg::Rescan) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
            continue;
        }

        let volumes = usb::discover_volumes_with_marker(explicit.as_deref(), &marker);
        let labels: Vec<String> = volumes
            .iter()
            .map(|volume| {
                format!(
                    "{} · {} file{}",
                    volume.root.display(),
                    volume.files.len(),
                    if volume.files.len() == 1 { "" } else { "s" }
                )
            })
            .collect();
        if last_volumes.as_ref() != Some(&labels) {
            last_volumes = Some(labels.clone());
            let _ = tx.send(UsbEvent::Volumes(labels.clone()));
            let status = if volumes.is_empty() {
                "no camera volume mounted (read-only import)".to_owned()
            } else {
                format!(
                    "watching {} volume(s); camera volume is read-only",
                    volumes.len()
                )
            };
            let _ = tx.send(UsbEvent::Status(status));
        }

        for volume in &volumes {
            for file in &volume.files {
                let key = file.clone();
                let Some(stamp) = file_stamp(file) else {
                    continue;
                };
                if seen.get(&key) == Some(&stamp) {
                    continue;
                }

                // Wait for a stable size + mtime, then read in one pass.
                let before = stamp;
                thread::sleep(STABILISE_DELAY);
                let Some(after) = file_stamp(file) else {
                    continue;
                };
                if before != after {
                    continue;
                }

                let mut attempt = 0;
                loop {
                    match usb::import_file(file, &cache, &mut index) {
                        Ok(outcome) => {
                            seen.insert(key.clone(), after);
                            if outcome.duplicate {
                                let _ = tx.send(UsbEvent::Log {
                                    level: Level::Info,
                                    text: format!(
                                        "USB: {} already indexed by content hash",
                                        file.display()
                                    ),
                                });
                            } else {
                                let plane = Arc::new(outcome.image.temperature_c.clone());
                                let _ = tx.send(UsbEvent::Imported(Box::new(ImportedUsb {
                                    entry: outcome.entry.clone(),
                                    plane,
                                    thumbnail_rgba: outcome.image.thumbnail_rgba.clone(),
                                    duplicate: false,
                                })));
                                let stats = outcome.image.stats;
                                let _ = tx.send(UsbEvent::Log {
                                    level: Level::Info,
                                    text: format!(
                                        "USB imported {}: {:.2}..{:.2} °C, hash {}",
                                        outcome.image.file_name,
                                        stats.min,
                                        stats.max,
                                        &outcome.entry.sha256[..12.min(outcome.entry.sha256.len())]
                                    ),
                                });
                            }
                            break;
                        }
                        Err(err) if attempt == 0 => {
                            attempt += 1;
                            thread::sleep(RETRY_DELAY);
                            let _ = tx.send(UsbEvent::Log {
                                level: Level::Warn,
                                text: format!(
                                    "USB: {} unreadable ({err}); retrying once",
                                    file.display()
                                ),
                            });
                        }
                        Err(err) => {
                            seen.insert(key.clone(), after);
                            let text = format!("USB: {} unreadable: {err}", file.display());
                            let _ = tx.send(UsbEvent::Error(text.clone()));
                            let _ = tx.send(UsbEvent::Log {
                                level: Level::Error,
                                text,
                            });
                            break;
                        }
                    }
                }
            }
        }

        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(UsbMsg::Shutdown) => return,
            Ok(UsbMsg::Rescan) => {
                seen.clear();
                let _ = tx.send(UsbEvent::Status("rescan requested".to_owned()));
            }
            Ok(UsbMsg::SetPaused(next)) => paused = next,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn emit_existing(tx: &Sender<UsbEvent>, cache: &Path, index: &UsbIndex) {
    for entry in index.entries.values() {
        let path = cache.join(&entry.cache_file);
        match usb::parse_usb_file(&path) {
            Ok(image) => {
                let _ = tx.send(UsbEvent::Imported(Box::new(ImportedUsb {
                    entry: entry.clone(),
                    plane: Arc::new(image.temperature_c),
                    thumbnail_rgba: image.thumbnail_rgba,
                    duplicate: true,
                })));
            }
            Err(err) => {
                let _ = tx.send(UsbEvent::Log {
                    level: Level::Warn,
                    text: format!("USB cache entry {} is unreadable: {err}", path.display()),
                });
            }
        }
    }
}

fn file_stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Some((metadata.len(), modified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usb::tests::synthetic_container;

    #[test]
    fn imports_a_new_file_and_emits_it() {
        let root = std::env::temp_dir().join(format!(
            "utitherm-usb-worker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let dcim = root.join("DCIM");
        std::fs::create_dir_all(&dcim).expect("dcim");
        std::fs::write(root.join("AUTORUN.INF"), "[autorun]\nlabel=UNIT UTi260B+\n")
            .expect("autorun");
        std::fs::write(dcim.join("202610240001E.jpg"), synthetic_container()).expect("file");

        let cache = root.join("cache");
        let worker = UsbWorker::spawn(
            cache,
            Some(root.clone()),
            crate::usb::USB_AUTORUN_MARKER.to_owned(),
        )
        .expect("spawn");
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let mut imported = None;
        while std::time::Instant::now() < deadline && imported.is_none() {
            for event in worker.try_events() {
                if let UsbEvent::Imported(entry) = event {
                    imported = Some(entry);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let imported = imported.expect("USB import event");
        assert_eq!(imported.entry.metadata.firmware, "1.1.9");
        assert_eq!(imported.plane.len(), crate::capture::TEMP_PLANE_SAMPLES);
        drop(worker);
        let _ = std::fs::remove_dir_all(&root);
    }
}
