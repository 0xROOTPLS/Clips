//! Persisted configuration (%APPDATA%\InstantReplay\config.cfg, key=value lines)
//! plus the lock-free runtime view shared across pipeline threads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use windows::Win32::UI::Input::KeyboardAndMouse::MOD_ALT;
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

pub const RES_CHOICES: [u32; 4] = [0, 1440, 1080, 720]; // 0 = native
pub const QUALITY_CHOICES: [(u32, &str); 3] = [(25, "High"), (15, "Medium"), (8, "Low")]; // Mbps

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub clip_seconds: u32,  // 15 | 30 | 60
    pub target_height: u32, // 0 = native, else downscale to this height
    pub monitor: u32,       // 0 = primary (default), else 1-based monitor index
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub gop_seconds: u32,
    pub capture_cursor: bool,
    pub backend: String, // "auto" | "wgc" | "dxgi" (auto: WGC unless yellow border is non-removable)
    pub mic: String,     // "off" | "default" | endpoint device id
    pub clips_dir: PathBuf,
    pub hotkey_mods: u32,
    pub hotkey_vk: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            clip_seconds: 30,
            target_height: 1080,
            monitor: 0,
            fps: 60,
            bitrate_mbps: 15,
            gop_seconds: 2,
            capture_cursor: true,
            backend: "auto".into(),
            mic: "default".into(),
            clips_dir: known_folder(&FOLDERID_Videos)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Clips"),
            hotkey_mods: MOD_ALT.0 as u32,
            hotkey_vk: b'C' as u32,
        }
    }
}

fn known_folder(id: &windows_core::GUID) -> Option<PathBuf> {
    unsafe {
        let pw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = pw.to_string().ok()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(pw.as_ptr() as *const _));
        Some(PathBuf::from(s))
    }
}

pub fn config_dir() -> PathBuf {
    known_folder(&FOLDERID_RoamingAppData)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("InstantReplay")
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.cfg")
    }

    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Config::default(),
        }
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "clip_seconds" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if matches!(n, 15 | 30 | 60) {
                            c.clip_seconds = n;
                        }
                    }
                }
                "target_height" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if n == 0 || matches!(n, 720 | 1080 | 1440 | 2160) {
                            c.target_height = n;
                        }
                    }
                }
                "monitor" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if n <= 8 {
                            c.monitor = n;
                        }
                    }
                }
                // Pre-submenu config compatibility.
                "native_resolution" => c.target_height = if v == "true" { 0 } else { 1080 },
                "fps" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if (10..=240).contains(&n) {
                            c.fps = n;
                        }
                    }
                }
                "bitrate_mbps" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if (1..=100).contains(&n) {
                            c.bitrate_mbps = n;
                        }
                    }
                }
                "gop_seconds" => {
                    if let Ok(n) = v.parse::<u32>() {
                        if (1..=10).contains(&n) {
                            c.gop_seconds = n;
                        }
                    }
                }
                "capture_cursor" => c.capture_cursor = v == "true",
                "backend" => {
                    if matches!(v, "auto" | "wgc" | "dxgi") {
                        c.backend = v.to_string();
                    }
                }
                "mic" => {
                    if !v.is_empty() {
                        c.mic = v.to_string();
                    }
                }
                "clips_dir" => {
                    if !v.is_empty() {
                        c.clips_dir = PathBuf::from(v);
                    }
                }
                "hotkey_mods" => {
                    if let Ok(n) = v.parse::<u32>() {
                        c.hotkey_mods = n;
                    }
                }
                "hotkey_vk" => {
                    if let Ok(n) = v.parse::<u32>() {
                        c.hotkey_vk = n;
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn serialize(&self) -> String {
        format!(
            "# Instant Replay configuration\n\
             clip_seconds={}\n\
             target_height={}\n\
             monitor={}\n\
             fps={}\n\
             bitrate_mbps={}\n\
             gop_seconds={}\n\
             capture_cursor={}\n\
             backend={}\n\
             mic={}\n\
             clips_dir={}\n\
             hotkey_mods={}\n\
             hotkey_vk={}\n",
            self.clip_seconds,
            self.target_height,
            self.monitor,
            self.fps,
            self.bitrate_mbps,
            self.gop_seconds,
            self.capture_cursor,
            self.backend,
            self.mic,
            self.clips_dir.display(),
            self.hotkey_mods,
            self.hotkey_vk,
        )
    }

    /// Atomic save: write temp file then rename over the target.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("cfg.tmp");
        std::fs::write(&tmp, self.serialize())?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Target exists on some filesystems: replace.
                std::fs::remove_file(path)?;
                std::fs::rename(&tmp, path)
            }
        }
    }
}

/// Lock-free runtime view shared by tray, supervisor, and pipeline threads.
pub struct RuntimeConfig {
    pub clip_seconds: AtomicU32,
    pub target_height: AtomicU32,
    pub monitor: AtomicU32,
    pub backend: AtomicU32, // 0 auto, 1 wgc, 2 dxgi
    pub capture_cursor: AtomicBool,
    pub fps: AtomicU32,
    pub bitrate_mbps: AtomicU32,
    pub gop_seconds: AtomicU32,
    pub mic: Mutex<String>,
    pub clips_dir: Mutex<PathBuf>,
}

impl RuntimeConfig {
    pub fn from(c: &Config) -> Self {
        RuntimeConfig {
            clip_seconds: AtomicU32::new(c.clip_seconds),
            target_height: AtomicU32::new(c.target_height),
            monitor: AtomicU32::new(c.monitor),
            backend: AtomicU32::new(match c.backend.as_str() {
                "wgc" => 1,
                "dxgi" => 2,
                _ => 0,
            }),
            capture_cursor: AtomicBool::new(c.capture_cursor),
            fps: AtomicU32::new(c.fps),
            bitrate_mbps: AtomicU32::new(c.bitrate_mbps),
            gop_seconds: AtomicU32::new(c.gop_seconds),
            mic: Mutex::new(c.mic.clone()),
            clips_dir: Mutex::new(c.clips_dir.clone()),
        }
    }

    pub fn mic(&self) -> String {
        self.mic.lock().map(|m| m.clone()).unwrap_or_else(|_| "off".into())
    }

    pub fn clip_seconds(&self) -> u32 {
        self.clip_seconds.load(Ordering::Relaxed)
    }
    pub fn target_height(&self) -> u32 {
        self.target_height.load(Ordering::Relaxed)
    }
    pub fn monitor(&self) -> u32 {
        self.monitor.load(Ordering::Relaxed)
    }
    pub fn backend(&self) -> u32 {
        self.backend.load(Ordering::Relaxed)
    }
    pub fn capture_cursor(&self) -> bool {
        self.capture_cursor.load(Ordering::Relaxed)
    }
    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::Relaxed)
    }
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_mbps.load(Ordering::Relaxed) * 1_000_000
    }
    pub fn gop_seconds(&self) -> u32 {
        self.gop_seconds.load(Ordering::Relaxed)
    }
    pub fn clips_dir(&self) -> PathBuf {
        self.clips_dir.lock().map(|p| p.clone()).unwrap_or_else(|_| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.clip_seconds = 60;
        c.target_height = 0;
        c.monitor = 2;
        c.bitrate_mbps = 20;
        c.backend = "dxgi".into();
        c.mic = "{0.0.1.00000000}.{abc}".into();
        c.clips_dir = PathBuf::from("D:\\my clips");
        let parsed = Config::parse(&c.serialize());
        assert_eq!(c, parsed);
    }

    #[test]
    fn legacy_native_resolution_key() {
        assert_eq!(Config::parse("native_resolution=true\n").target_height, 0);
        assert_eq!(Config::parse("native_resolution=false\n").target_height, 1080);
        assert_eq!(Config::parse("target_height=999\n").target_height, 1080); // invalid → default
        assert_eq!(Config::parse("monitor=99\n").monitor, 0);
    }

    #[test]
    fn defaults_on_garbage() {
        let c = Config::parse("clip_seconds=99\nfps=banana\n=??\nrandom line\nbitrate_mbps=0\nbackend=gdi\n");
        let d = Config::default();
        assert_eq!(c.clip_seconds, d.clip_seconds); // 99 not in {15,30,60}
        assert_eq!(c.fps, d.fps);
        assert_eq!(c.bitrate_mbps, d.bitrate_mbps); // 0 out of range
        assert_eq!(c.backend, "auto"); // unknown backend rejected
    }

    #[test]
    fn empty_input_is_default() {
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn tolerates_whitespace_and_comments() {
        let c = Config::parse("# comment\n  clip_seconds = 15  \n\ncapture_cursor=false\n");
        assert_eq!(c.clip_seconds, 15);
        assert!(!c.capture_cursor);
    }
}
