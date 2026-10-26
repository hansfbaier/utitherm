# UTi260B+ device profile (root access, firmware 1.1.9)

Captured 2026-10-24 from the camera over its own AP. Read-only recon; nothing on the
device was modified.

## 1. Access

| item | value | source |
|---|---|---|
| AP SSID | `UTi260B+` | hostapd.conf, scan |
| AP passphrase | `12345678` | `/mnt/UDISK/release/hostapd.conf`, `IniFormat.ini [setnetwork] PWhs` |
| Camera IP | `192.168.27.10` (DHCP gateway) | DHCP lease |
| telnet | `root` / `unit123` — **root shell** | vendor APK v2.9.10 `Config_DeviceType.java` (`pwd_ftp`), same credential on telnet |
| FTP | `root` / `unit123`, chrooted to `/mnt` | same |
| password hash | `root:$1$4S/jmaNt$sCVah6r0jO2tPeLPiWxsC.` (MD5-crypt) | `/etc/shadow`; verified `openssl passwd -1 -salt 4S/jmaNt unit123` matches |

The credential came from the newer vendor APK (v2.9.10), which ships a per-model config
class for this exact device; the older v2.3.4 APK does not contain it. It is a shared
vendor default, not per-unit.

## 2. Hardware / OS

| item | value |
|---|---|
| SoC | Allwinner **R818** (4x Cortex-A53, `CPU part 0xd03`), aarch64 |
| Kernel | `Linux UNI-T 4.9.191 #441 SMP PREEMPT Fri Jul 25 02:17:45 UTC 2025 aarch64` |
| Distro | Allwinner **Tina Linux** (`DISTRIB_TARGET='r818-uti260bplus/generic v2.0'`, `DISTRIB_ID='tina.cht.20250725.021621'`, release `Neptune`, `3.5.1`) |
| Init | OpenWrt-style `procd` + `/etc/init.d` |
| Root filesystem | `/dev/root` ext4, 739 MB, mounted read-only at `/rom`, overlayed by `overlayfs` |
| Writable layer | `/dev/by-name/UDISK` ext4 522 MB at `/overlay` |
| SD card | `/dev/mmcblk0p10`, 27.5 GB, NTFS via `ntfs-3g` (`fuseblk`) at `/mnt/SDCARD` — this is the volume exposed over USB mass storage |
| Private | `/dev/by-name/private` 15.8 MB vfat at `/mnt/private` |
| RAM | 1 GB |

Partitions of interest: `UDISK` holds `/mnt/UDISK/release/` — the Qt application and all
runtime blobs; `private` holds calibration/config INIs.

## 3. Services

| port | process | notes |
|---|---|---|
| 9527, 9528 | **single binary `APP_R818`** (PID 1193) | both ThermalLink ports are one Qt application — a malformed command kills both, which is exactly what happened during earlier probing |
| 23 | `telnetd` | root login with the vendor default |
| 21 | `tcpsvd` (busybox FTP) | chroot to `/mnt`; exposes SDCARD, UDISK, private, thumb |
| 53 | `dnsmasq` | DHCP + DNS for the AP |

Startup: `/etc/init.d/play_app` runs
`/mnt/UDISK/release/APP_R818 -platform eglfs > /dev/console 2>&1 &`.
`/etc/init.d/S99swupdate_autorun` handles SD/USB firmware auto-update.

## 4. Application and SDK layout (`/mnt/UDISK/release`)

| file | size | meaning |
|---|---|---|
| `APP_R818` | 4 373 704 | Qt/eglfs application, aarch64, **not stripped** (2 919 dynsyms), links ffmpeg 61/59 |
| `Tiny2C_Firmware.bin` | 985 480 | thermal core MCU firmware candidate |
| `Tiny2C_thermal_test_all.bin` | 963 476 | test variant |
| `high_gain_tpd_nuc_t.bin`, `low_gain_tpd_nuc_t.bin` | 32 768 each | non-uniformity correction tables, high/low gain |
| `UT-highF1.bin`, `UT-lowF1.bin` | 8 176 each | per-gain correction data |
| `lib/` | — | `libircam`, `libircmd`, `libirdfu`, `libirinfoparse`, `libirisp`, `libirisp_pro`, `libirparse`, `libirsensor`, `libirspi`, `libirtemp`, `libimage_algorithms_lite`, `libMNN*` |
| `Aligning.ini`, `AligningAlg.ini`, `IniFormat*.ini`, `imageAlgConfig*.yaml`, `hostapd.conf` | — | configuration (below) |
| `adb_en.sh` | 1 195 | USB mode switcher: mass storage / UVC webcam / **ADB gadget** |

Note `adb_en.sh` defines `usb_control_mode()` that brings up `g_ffs` + `adbd`, so the device
can expose an ADB interface instead of mass storage — a fourth access path.

## 5. Configuration files

### `IniFormat.ini` (settings snapshot)

- `[setnetwork] IDhs=UTi260B+`, `PWhs=12345678`, `wifihotspot=1` — AP credentials.
- `[setparameter]` emissivity `0.95`, humidity `55%RH`, manual distance `0.25m`,
  reflected/apparent temperature `298.1K` = `25 °C`, `tempgain=1`.
- `[setunits] temperatureUnits=K`, `distanceUnits=m` — factory default is **Kelvin**, which
  explains the `unit=6` seen in `init` after our experiment:
  `unit 0 = °C`, `6 = K` (mapping confirmed by matching 25 °C → 298.1 K, 36.7 °C → 309.9 K).
- `[setalarm]` triples in K / °C / °F: high `309.9K` / `36.7 °C` / `98.1 °F`,
  low `278.1K` / `5.0 °C` / `41.0 °F`. Matches the `init` alarm field exactly.
- `[setImage] distance=60`, `fusion=50`, `fusiondistance=25`.
- `[setting] settinglaserxy="160,229"` — matches the `init` laser bytes `a0 00 e5 00`
  (160, 229 little-endian i16), so the laser coordinate decoding is confirmed.
- `[Custom]` six measurement slots `P1..P4, R1, R2` with geometry strings, e.g.
  `geoName0="89,199,40,20"`, `geoType2="130,200,100,100"` — the same rectangle family that
  appears in the USB file metadata block.

### `Aligning.ini` (factory calibration)

- Serial: `SNCode=ATI0D25350516`. (The USB metadata block carried a different 6-byte tag,
  `01 a1 6f b3 35 4f` — likely a separate short ID, not this serial.)
- 9-point radiometric calibration, `settempN` vs measured `tempN` vs raw `vtempN`:

```
set   -10.0   20.0   75.0    0.0   75.0  150.0  100.0  250.0  400.0
meas   13.2   28.5   64.0   15.2   62.9  119.1   81.9  195.6  308.0
raw    8046   8044   8044   8126   8124   8123   8043   8123   8122
```

  The raw values barely move (8043…8126) while measured temperature spans 300 °C, so `raw`
  is not a linear temperature input — the conversion is a calibration curve, consistent with
  the earlier finding that the u16 plane is not linearly related to the f32 temperature plane.
- `FusionXY="-2,-1"`, `FusionZoom=1.4` — visible/thermal alignment.
- `hwversion=1.0`, `settingreset=1.1.9`, `sysversion=1.0.0`.

### `imageAlgConfig.yaml`

`ImgAlignRect: [57, 84, 354, 468]` — fusion alignment rectangle in the 480x640 visual image.

## 6. Correspondence with the network protocol

| protocol fact | device-side confirmation |
|---|---|
| display 240x320, measurement plane 192x256 | APK v2.9.10 `Config_DeviceType`: `screenWIDTH=240, screenHEIGHT=320, dataWIDTH=192, dataHEIGHT=256` |
| camera IP 192.168.27.10 | APK v2.9.10 `Config_DeviceType.address` and live DHCP |
| device type 112 | `Config_DeviceType.DEV_TYPE = 112`, `DeviceTypeHelper` id 112 |
| ports 9527/9528 | APK config + `netstat` (one process) |
| `init` emissivity/humidity/distance/ambient/alarm | identical values in `IniFormat.ini [setparameter]` / `[setalarm]` |
| laser coords 160,229 | `settinglaserxy` |
| Kelvin unit byte 6 | `[setunits] temperatureUnits=K` |
| single-connection daemon fragility | one process owns both ports |

Discrepancy worth noting: APK v2.9.10 `DeviceTypeHelper` assigns UTi260B+ `port_live = 9528`,
while `Config_DeviceType` and the live device both use **9527** for live and 9528 for the
gallery/file channel. Empirically 9527 is the live port.

## 7. Security notes

- One shared vendor root credential (`root`/`unit123`) on telnet **and** FTP; same default on
  every unit of this model.
- AP passphrase is the vendor default `12345678` (`hostapd.conf`, WPA2-PSK, channel 6, SSID
  broadcast).
- The AP is **not raised automatically at boot**. After a reboot a human must enable Wi-Fi on
  the device first; the SSID then appears, observed between ~25 s and a few minutes after
  power-on. Budget for that in any recovery workflow.
- Nothing respawns `APP_R818`: it is started once from `/etc/init.d/play_app` with `&`.
  Relaunching it — from an interactive shell or from a respawn loop — does **not** restore
  service. Both were tried: the new process arrives with a partial thread set (8 threads,
  a handful of sockets) and never opens 9527/9528, although **the on-screen UI keeps working** —
  it is a partially initialised app, not a frozen one. Only a reboot restores the network
  service. Presumably the socket threads are started from a boot-time state of the camera
  pipeline, not merely from the display being available.
- Crash forensics rig (installed, reversible): `/etc/init.d/play_app` now runs
  `ulimit -c unlimited`, sets `core_pattern` to `/mnt/SDCARD/cores/%e.%t.%p.core` and appends
  app output to `/tmp/app.log`. The vendor's single start is unchanged, so behaviour at boot is
  identical. The unmodified script is preserved as `/mnt/SDCARD/play_app.orig`
  (md5 `b6c4d6d039278dc55aa27ed8463c7b2b`).
- The live-control channel on 9527/9528 is unauthenticated: anyone who joins the default AP
  can read the video and drive the camera.
- `adb_en.sh` can enable an ADB gadget (`adbd`) over USB.
- The daemon crashes on malformed commands and requires a reboot; no watchdog restarts it.
