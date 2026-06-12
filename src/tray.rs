//! System tray UI: hidden window, notify icon, context menu, global hotkey.
//! Runs the message loop on the main thread. Other threads request balloon
//! notifications through `TrayHandle` (queue + PostMessage).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR,
    NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW,
    TrackPopupMenu, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, IDI_APPLICATION,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, SW_SHOWNORMAL, TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_COMMAND, WM_DESTROY, WM_DISPLAYCHANGE, WM_HOTKEY,
    WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use crate::config::{Config, RuntimeConfig};
use crate::supervisor::Ctl;

const WM_APP_TRAY: u32 = 0x8000 + 1; // WM_APP + 1
const WM_APP_BALLOON: u32 = 0x8000 + 2;

const HOTKEY_ID: i32 = 1;
const TRAY_UID: u32 = 1;

const IDM_SAVE: usize = 1001;
const IDM_OPEN_FOLDER: usize = 1002;
const IDM_LEN_15: usize = 1011;
const IDM_LEN_30: usize = 1012;
const IDM_LEN_60: usize = 1013;
const IDM_CURSOR: usize = 1022;
const IDM_AUTOSTART: usize = 1031;
const IDM_RES_BASE: usize = 1040; // + index into RES_CHOICES
const IDM_Q_BASE: usize = 1050; // + index into QUALITY_CHOICES
const IDM_MON_BASE: usize = 1060; // +0 = default (primary), +n = monitor n
const IDM_MIC_BASE: usize = 1070; // +0 = off, +1 = default, +2+i = device i
const MIC_MAX_DEVICES: usize = 16;
const IDM_QUIT: usize = 1099;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: windows::core::PCWSTR = w!("InstantReplay");

fn autostart_enabled() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    unsafe {
        let key = wide(RUN_KEY);
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

fn set_autostart(on: bool) {
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
    };
    unsafe {
        let key = wide(RUN_KEY);
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let val = wide(&format!("\"{}\"", exe.display()));
                let r = RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key.as_ptr()),
                    RUN_VALUE,
                    REG_SZ.0,
                    Some(val.as_ptr() as *const _),
                    (val.len() * 2) as u32,
                );
                crate::log!("tray: autostart on: {r:?}");
            }
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), RUN_VALUE);
            crate::log!("tray: autostart off: {r:?}");
        }
    }
}

struct Balloon {
    title: String,
    text: String,
    error: bool,
}

struct TrayState {
    tx: Sender<Ctl>,
    runtime: Arc<RuntimeConfig>,
    balloons: Arc<Mutex<VecDeque<Balloon>>>,
    taskbar_created_msg: u32,
    hotkey_desc: String,
    mic_devices: Vec<(String, String)>, // (id, name), refreshed at menu open
}

/// Cheap cloneable handle for other threads to surface notifications.
#[derive(Clone)]
pub struct TrayHandle {
    hwnd: isize,
    balloons: Arc<Mutex<VecDeque<Balloon>>>,
}

// SAFETY: HWND is used only with thread-safe PostMessageW.
unsafe impl Send for TrayHandle {}
unsafe impl Sync for TrayHandle {}

impl TrayHandle {
    pub fn notify(&self, title: &str, text: &str, error: bool) {
        if let Ok(mut q) = self.balloons.lock() {
            q.push_back(Balloon { title: title.into(), text: text.into(), error });
            if q.len() > 8 {
                q.pop_front();
            }
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_APP_BALLOON,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

pub struct Tray {
    hwnd: HWND,
    state: *mut TrayState,
}

impl Tray {
    /// Create the hidden window, tray icon, and hotkey. Must be called on the
    /// thread that will run `run_message_loop` (the main thread).
    pub fn create(tx: Sender<Ctl>, runtime: Arc<RuntimeConfig>) -> Result<Tray> {
        unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let class_name = w!("ClipsInstantReplayTray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("Instant Replay"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;

            // Register the hotkey; fall back to Ctrl+Alt+F9 on conflict.
            let cfg_mods = HOT_KEY_MODIFIERS(runtime_hotkey_mods(&runtime));
            let cfg_vk = runtime_hotkey_vk(&runtime);
            let mut hotkey_desc = hotkey_name(cfg_mods, cfg_vk);
            if RegisterHotKey(Some(hwnd), HOTKEY_ID, cfg_mods | MOD_NOREPEAT, cfg_vk).is_err() {
                let fb_mods = MOD_CONTROL | MOD_ALT;
                let fb_vk = 0x78; // VK_F9
                RegisterHotKey(Some(hwnd), HOTKEY_ID, fb_mods | MOD_NOREPEAT, fb_vk)?;
                crate::log!("tray: hotkey {} taken, fell back to Ctrl+Alt+F9", hotkey_desc);
                hotkey_desc = "Ctrl+Alt+F9".into();
            }

            let state = Box::into_raw(Box::new(TrayState {
                tx,
                runtime,
                balloons: Arc::new(Mutex::new(VecDeque::new())),
                taskbar_created_msg: RegisterWindowMessageW(w!("TaskbarCreated")),
                hotkey_desc,
                mic_devices: Vec::new(),
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);

            add_tray_icon(hwnd, &(*state).hotkey_desc);
            crate::log!("tray: icon added, hotkey {}", (*state).hotkey_desc);
            Ok(Tray { hwnd, state })
        }
    }

    pub fn handle(&self) -> TrayHandle {
        unsafe {
            TrayHandle {
                hwnd: self.hwnd.0 as isize,
                balloons: (*self.state).balloons.clone(),
            }
        }
    }

    /// Blocks until WM_QUIT.
    pub fn run_message_loop(&self) {
        unsafe {
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            remove_tray_icon(self.hwnd);
            // State box is freed in WM_DESTROY if the window died first;
            // otherwise destroy the window now (frees it).
            if GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) != 0 {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn runtime_hotkey_mods(_rt: &RuntimeConfig) -> u32 {
    // Hotkey is fixed at startup from persisted config; RuntimeConfig doesn't
    // carry it. Read the file-backed value once here.
    Config::load(&Config::path()).hotkey_mods
}

fn runtime_hotkey_vk(_rt: &RuntimeConfig) -> u32 {
    Config::load(&Config::path()).hotkey_vk
}

fn hotkey_name(mods: HOT_KEY_MODIFIERS, vk: u32) -> String {
    let mut s = String::new();
    if mods.0 & MOD_CONTROL.0 != 0 {
        s.push_str("Ctrl+");
    }
    if mods.0 & MOD_ALT.0 != 0 {
        s.push_str("Alt+");
    }
    if mods.0 & 0x4 != 0 {
        s.push_str("Shift+");
    }
    match vk {
        0x70..=0x87 => s.push_str(&format!("F{}", vk - 0x6F)),
        0x30..=0x5A => s.push(vk as u8 as char),
        other => s.push_str(&format!("0x{other:02X}")),
    }
    s
}

fn copy_wstr(dst: &mut [u16], s: &str) {
    let mut n = 0;
    for u in s.encode_utf16() {
        if n >= dst.len() - 1 {
            break;
        }
        dst[n] = u;
        n += 1;
    }
    dst[n] = 0;
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid
}

fn add_tray_icon(hwnd: HWND, hotkey: &str) {
    unsafe {
        let mut nid = base_nid(hwnd);
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        // Embedded resource id 1 (app.rc); stock icon as fallback.
        let hinstance = GetModuleHandleW(None).map(|h| h.into()).ok();
        nid.hIcon = LoadIconW(hinstance, PCWSTR(1 as *const u16))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default();
        copy_wstr(&mut nid.szTip, &format!("Instant Replay — {hotkey} saves clip"));
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let nid = base_nid(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn show_balloon(hwnd: HWND, b: &Balloon) {
    unsafe {
        let mut nid = base_nid(hwnd);
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = if b.error { NIIF_ERROR } else { NIIF_INFO };
        copy_wstr(&mut nid.szInfoTitle, &b.title);
        copy_wstr(&mut nid.szInfo, &b.text);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

unsafe fn state<'a>(hwnd: HWND) -> Option<&'a mut TrayState> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
    if p.is_null() {
        None
    } else {
        Some(&mut *p)
    }
}

fn persist<F: FnOnce(&mut Config)>(f: F) {
    let path = Config::path();
    let mut c = Config::load(&path);
    f(&mut c);
    if let Err(e) = c.save(&path) {
        crate::log!("config: save failed: {e}");
    }
}

unsafe fn show_menu(hwnd: HWND, st: &mut TrayState) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let len = st.runtime.clip_seconds();
    let _ = AppendMenuW(menu, MF_STRING, IDM_SAVE, PCWSTR(wide(&format!("Save clip\t{}", st.hotkey_desc)).as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_FOLDER, w!("Open clips folder"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    // Clip length submenu.
    if let Ok(sub) = CreatePopupMenu() {
        let _ = AppendMenuW(sub, MF_STRING | check(len == 15), IDM_LEN_15, w!("15 s"));
        let _ = AppendMenuW(sub, MF_STRING | check(len == 30), IDM_LEN_30, w!("30 s"));
        let _ = AppendMenuW(sub, MF_STRING | check(len == 60), IDM_LEN_60, w!("60 s"));
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Clip length"));
    }
    // Resolution submenu.
    if let Ok(sub) = CreatePopupMenu() {
        let cur = st.runtime.target_height();
        for (i, h) in crate::config::RES_CHOICES.iter().enumerate() {
            let label = if *h == 0 { "Native".to_string() } else { format!("{h}p") };
            let _ = AppendMenuW(sub, MF_STRING | check(cur == *h), IDM_RES_BASE + i, PCWSTR(wide(&label).as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Resolution"));
    }
    // Quality submenu (encoder bitrate).
    if let Ok(sub) = CreatePopupMenu() {
        let cur = st.runtime.bitrate_mbps.load(std::sync::atomic::Ordering::Relaxed);
        for (i, (mbps, name)) in crate::config::QUALITY_CHOICES.iter().enumerate() {
            let label = format!("{name} ({:.1} MB/s)", *mbps as f64 / 8.0);
            let _ = AppendMenuW(sub, MF_STRING | check(cur == *mbps), IDM_Q_BASE + i, PCWSTR(wide(&label).as_ptr()));
        }
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Quality"));
    }
    // Microphone submenu.
    if let Ok(sub) = CreatePopupMenu() {
        let cur = st.runtime.mic();
        st.mic_devices = crate::audio::list_capture_devices();
        st.mic_devices.truncate(MIC_MAX_DEVICES);
        let _ = AppendMenuW(sub, MF_STRING | check(cur == "off"), IDM_MIC_BASE, w!("No microphone"));
        let _ = AppendMenuW(sub, MF_STRING | check(cur == "default"), IDM_MIC_BASE + 1, w!("Default microphone"));
        for (i, (id, name)) in st.mic_devices.iter().enumerate() {
            let _ = AppendMenuW(
                sub,
                MF_STRING | check(cur == *id),
                IDM_MIC_BASE + 2 + i,
                PCWSTR(wide(name).as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Microphone"));
    }
    // Monitor submenu.
    if let Ok(sub) = CreatePopupMenu() {
        let cur = st.runtime.monitor();
        let _ = AppendMenuW(sub, MF_STRING | check(cur == 0), IDM_MON_BASE, w!("Default (primary)"));
        for m in crate::d3d::list_monitors().into_iter().take(8) {
            let label = format!(
                "{}: {} {}x{}{}",
                m.index,
                m.name.trim_start_matches("\\\\.\\"),
                m.width,
                m.height,
                if m.primary { " (primary)" } else { "" }
            );
            let _ = AppendMenuW(
                sub,
                MF_STRING | check(cur == m.index),
                IDM_MON_BASE + m.index as usize,
                PCWSTR(wide(&label).as_ptr()),
            );
        }
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("Monitor"));
    }

    let _ = AppendMenuW(menu, MF_STRING | check(st.runtime.capture_cursor()), IDM_CURSOR, w!("Capture cursor"));
    let _ = AppendMenuW(menu, MF_STRING | check(autostart_enabled()), IDM_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, w!("Quit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
    let _ = DestroyMenu(menu);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn on_command(st: &mut TrayState, id: usize) {
    use std::sync::atomic::Ordering;
    match id {
        IDM_SAVE => {
            let _ = st.tx.send(Ctl::SaveClip);
        }
        IDM_OPEN_FOLDER => {
            let dir = st.runtime.clips_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_folder(&dir);
        }
        IDM_LEN_15 | IDM_LEN_30 | IDM_LEN_60 => {
            let n = match id {
                IDM_LEN_15 => 15,
                IDM_LEN_30 => 30,
                _ => 60,
            };
            st.runtime.clip_seconds.store(n, Ordering::Relaxed);
            persist(|c| c.clip_seconds = n);
            let _ = st.tx.send(Ctl::SetClipLen(n));
        }
        _ if (IDM_RES_BASE..IDM_RES_BASE + crate::config::RES_CHOICES.len()).contains(&id) => {
            let h = crate::config::RES_CHOICES[id - IDM_RES_BASE];
            st.runtime.target_height.store(h, Ordering::Relaxed);
            persist(|c| c.target_height = h);
            let _ = st.tx.send(Ctl::VideoRestart("resolution changed"));
        }
        _ if (IDM_Q_BASE..IDM_Q_BASE + crate::config::QUALITY_CHOICES.len()).contains(&id) => {
            let mbps = crate::config::QUALITY_CHOICES[id - IDM_Q_BASE].0;
            st.runtime.bitrate_mbps.store(mbps, Ordering::Relaxed);
            persist(|c| c.bitrate_mbps = mbps);
            let _ = st.tx.send(Ctl::VideoRestart("quality changed"));
        }
        _ if (IDM_MIC_BASE..IDM_MIC_BASE + 2 + MIC_MAX_DEVICES).contains(&id) => {
            let sel = match id - IDM_MIC_BASE {
                0 => Some("off".to_string()),
                1 => Some("default".to_string()),
                n => st.mic_devices.get(n - 2).map(|(id, _)| id.clone()),
            };
            if let Some(sel) = sel {
                if let Ok(mut m) = st.runtime.mic.lock() {
                    *m = sel.clone();
                }
                persist(|c| c.mic = sel);
                let _ = st.tx.send(Ctl::AudioRestart("microphone changed"));
            }
        }
        _ if (IDM_MON_BASE..=IDM_MON_BASE + 8).contains(&id) => {
            let mon = (id - IDM_MON_BASE) as u32;
            st.runtime.monitor.store(mon, Ordering::Relaxed);
            persist(|c| c.monitor = mon);
            let _ = st.tx.send(Ctl::VideoRestart("monitor changed"));
        }
        IDM_CURSOR => {
            let now = !st.runtime.capture_cursor();
            st.runtime.capture_cursor.store(now, Ordering::Relaxed);
            persist(|c| c.capture_cursor = now);
            let _ = st.tx.send(Ctl::CursorChanged(now));
        }
        IDM_AUTOSTART => set_autostart(!autostart_enabled()),
        IDM_QUIT => {
            crate::log!("tray: quit requested");
            PostQuitMessage(0);
        }
        _ => {}
    }
}

fn open_folder(dir: &Path) {
    unsafe {
        let path = wide(&dir.display().to_string());
        ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_TRAY => {
            if let Some(st) = state(hwnd) {
                match lparam.0 as u32 {
                    WM_RBUTTONUP | WM_LBUTTONUP => show_menu(hwnd, st),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_BALLOON => {
            if let Some(st) = state(hwnd) {
                let b = st.balloons.lock().ok().and_then(|mut q| q.pop_front());
                if let Some(b) = b {
                    show_balloon(hwnd, &b);
                }
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            if let Some(st) = state(hwnd) {
                crate::log!("tray: hotkey pressed");
                let _ = st.tx.send(Ctl::SaveClip);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if let Some(st) = state(hwnd) {
                on_command(st, (wparam.0 & 0xFFFF) as usize);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            if let Some(st) = state(hwnd) {
                crate::log!("tray: WM_DISPLAYCHANGE");
                let _ = st.tx.send(Ctl::VideoRestart("display mode changed"));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
            if !p.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(p));
            }
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the taskbar is new, re-add our icon.
            if let Some(st) = state(hwnd) {
                if msg == st.taskbar_created_msg && msg != 0 {
                    add_tray_icon(hwnd, &st.hotkey_desc);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
