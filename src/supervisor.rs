//! Pipeline lifecycle owner: builds/rebuilds the video leg, restarts with
//! backoff on failure, snapshots the ring and spawns writer threads on save.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

use crate::config::RuntimeConfig;
use crate::encoder::Encoder;
use crate::frames::{FrameQueue, Pacer, QFrame};
use crate::ring::{Packet, Ring, StreamKind};
use crate::tray::TrayHandle;
use crate::{capture, convert, d3d, mux};

#[derive(Debug)]
pub enum Ctl {
    SaveClip,
    SetClipLen(u32),
    CursorChanged(bool),
    VideoRestart(&'static str),
    AudioRestart(&'static str),
    VideoLegDied(String),
    AudioLegDied(String),
    Quit,
}

const BACKOFF: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
const RING_BYTE_CAP: usize = 400 * 1024 * 1024;

/// Capture backend: WGC (borderless, Win11) or DXGI duplication (Win10 fallback).
pub(crate) enum Cap {
    Wgc(capture::Capture),
    Dupl(crate::dupl::DuplCapture),
}

impl Cap {
    fn set_cursor(&self, on: bool) {
        if let Cap::Wgc(c) = self {
            c.set_cursor(on);
        }
    }
    fn stop(&mut self) {
        match self {
            Cap::Wgc(c) => c.stop(),
            Cap::Dupl(c) => c.stop(),
        }
    }
    fn probe(&self) -> bool {
        match self {
            Cap::Wgc(c) => c.item.Size().is_ok(),
            Cap::Dupl(c) => c.probe(),
        }
    }
}

pub(crate) struct VideoLeg {
    d3d: d3d::D3D,
    cap: Cap,
    enc: Arc<Encoder>,
    queue: Arc<FrameQueue>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    last_output: Arc<AtomicI64>, // QPC 100 ns of last encoded sample
    up_since: Instant,
    out_w: u32,
    out_h: u32,
    fps: u32,
    bitrate: u32,
}

impl VideoLeg {
    pub(crate) fn mux_params(&self, audio: Option<crate::mux::AudioParams>) -> mux::MuxParams {
        mux::MuxParams {
            width: self.out_w,
            height: self.out_h,
            fps: self.fps,
            bitrate: self.bitrate,
            seq_header: self.enc.meta.lock().ok().and_then(|m| m.seq_header.clone()),
            audio,
        }
    }

    pub(crate) fn teardown(mut self) {
        self.cap.stop();
        self.stop.store(true, Ordering::Relaxed);
        self.enc.wake(&self.queue);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.enc.shutdown();
        crate::log!("video leg: torn down");
    }
}

pub(crate) fn build_video_leg(
    runtime: &RuntimeConfig,
    ring: &Arc<Mutex<Ring>>,
    tx: &Sender<Ctl>,
) -> windows::core::Result<VideoLeg> {
    // auto: WGC unless the yellow border can't be disabled (Win10) -> DXGI duplication.
    let use_dupl = match runtime.backend() {
        1 => false,
        2 => true,
        _ => !capture::border_removable(),
    };
    let d3d = d3d::create_for_monitor(runtime.monitor())?;
    let (item, dupl_out, in_w, in_h);
    if use_dupl {
        let (o, w, h) = crate::dupl::output_for(&d3d)?;
        (item, dupl_out, in_w, in_h) = (None, Some(o), w, h);
    } else {
        let it = capture::create_item(&d3d)?;
        let size = it.Size()?;
        (in_w, in_h) = (size.Width as u32, size.Height as u32);
        (item, dupl_out) = (Some(it), None);
    }
    let (out_w, out_h) = convert::output_size(in_w, in_h, runtime.target_height());
    let fps = runtime.fps();
    let bitrate = runtime.bitrate_bps();

    let mut converter = convert::Converter::new(&d3d, in_w, in_h, out_w, out_h, fps)?;
    let queue = Arc::new(FrameQueue::default());
    let enc = Arc::new(Encoder::new(&d3d, out_w, out_h, fps, bitrate, fps * runtime.gop_seconds())?);
    let stop = Arc::new(AtomicBool::new(false));

    let q2 = queue.clone();
    let mut pacer = Pacer::new(fps);
    let restart_sent = Arc::new(AtomicBool::new(false));
    let (rs1, rs2, rs3) = (restart_sent.clone(), restart_sent.clone(), restart_sent);
    let (tx_frame, tx_resize, tx_closed) = (tx.clone(), tx.clone(), tx.clone());
    let mut convert_fails = 0u32;
    // Backend-agnostic per-frame path: pace -> NV12 -> encoder queue.
    let mut handle = move |tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, pts: i64| {
        if let Some(snapped) = pacer.accept(pts) {
            match converter.convert(tex) {
                Ok(nv12) => {
                    convert_fails = 0;
                    q2.push(QFrame { tex: nv12, pts: snapped });
                }
                Err(e) => {
                    convert_fails += 1;
                    if convert_fails == 30 && !rs1.swap(true, Ordering::Relaxed) {
                        let _ = tx_frame.send(Ctl::VideoLegDied(format!("convert: {e}")));
                    }
                }
            }
        }
    };
    let on_closed = move || {
        if !rs2.swap(true, Ordering::Relaxed) {
            let _ = tx_closed.send(Ctl::VideoRestart("capture source closed"));
        }
    };
    let cap = if let Some(out) = dupl_out {
        Cap::Dupl(crate::dupl::DuplCapture::start(&d3d, out, in_w, in_h, fps, handle, on_closed)?)
    } else {
        Cap::Wgc(capture::Capture::start(
            &d3d,
            item.unwrap(),
            runtime.capture_cursor(),
            move |frame| {
                if let Ok(cs) = frame.ContentSize() {
                    if (cs.Width as u32, cs.Height as u32) != (in_w, in_h) {
                        if !rs3.swap(true, Ordering::Relaxed) {
                            let _ = tx_resize.send(Ctl::VideoRestart("content size changed"));
                        }
                        return;
                    }
                }
                if let Ok((tex, pts)) = capture::frame_texture(frame) {
                    handle(&tex, pts);
                }
            },
            on_closed,
        )?)
    };

    let last_output = Arc::new(AtomicI64::new(0));
    let (enc2, q3, stop2, ring2, tx2, hb) =
        (enc.clone(), queue.clone(), stop.clone(), ring.clone(), tx.clone(), last_output.clone());
    let thread = std::thread::Builder::new()
        .name("encoder".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let r = catch_unwind(AssertUnwindSafe(|| {
                enc2.run(&q3, &stop2, |f| {
                    hb.store(crate::frames::qpc_now(), Ordering::Relaxed);
                    if let Ok(mut ring) = ring2.lock() {
                        ring.push(Packet {
                            kind: StreamKind::Video,
                            pts: f.pts,
                            dur: f.dur,
                            keyframe: f.keyframe,
                            data: f.data.into(),
                        });
                    }
                })
            }));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = tx2.send(Ctl::VideoLegDied(format!("encoder: {e}")));
                }
                Err(_) => {
                    let _ = tx2.send(Ctl::VideoLegDied("encoder panicked".into()));
                }
            }
        })
        .expect("spawn encoder");

    crate::log!(
        "video leg: up ({}x{} @ {} fps, {:.1} MB/s, {})",
        out_w,
        out_h,
        fps,
        bitrate as f64 / 8e6,
        if use_dupl { "dxgi" } else { "wgc" }
    );
    Ok(VideoLeg {
        d3d,
        cap,
        enc,
        queue,
        stop,
        thread: Some(thread),
        last_output,
        up_since: Instant::now(),
        out_w,
        out_h,
        fps,
        bitrate,
    })
}

/// Audible save feedback: soft chime on success, critical-stop on failure.
fn play_feedback(ok: bool) {
    use windows::core::w;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    let alias = if ok { w!("SystemAsterisk") } else { w!("SystemHand") };
    unsafe {
        let _ = PlaySoundW(alias, None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT);
    }
}

struct Supervisor {
    runtime: Arc<RuntimeConfig>,
    tray: TrayHandle,
    tx: Sender<Ctl>,
    ring: Arc<Mutex<Ring>>,
    video: Option<VideoLeg>,
    video_backoff: usize,
    video_retry_at: Option<Instant>,
    audio: Option<crate::audio::AudioLeg>,
    audio_backoff: usize,
    audio_retry_at: Option<Instant>,
    audio_up_since: Option<Instant>,
    save_in_flight: Arc<AtomicBool>,
    pending_save: bool,
}

impl Supervisor {
    fn restart_video(&mut self, reason: &str) {
        crate::log!("supervisor: video restart ({reason})");
        if let Some(leg) = self.video.take() {
            leg.teardown();
        }
        // Encoder restart invalidates in-flight parameter sets.
        if let Ok(mut r) = self.ring.lock() {
            r.clear();
        }
        self.video_retry_at = Some(Instant::now());
    }

    fn try_build_video(&mut self) {
        match build_video_leg(&self.runtime, &self.ring, &self.tx) {
            Ok(leg) => {
                self.video = Some(leg);
                self.video_backoff = 0;
                self.video_retry_at = None;
            }
            Err(e) => {
                let delay = BACKOFF[self.video_backoff.min(BACKOFF.len() - 1)];
                self.video_backoff += 1;
                crate::log!("supervisor: video build failed ({e}), retry in {:?}", delay);
                self.video_retry_at = Some(Instant::now() + delay);
            }
        }
    }

    fn restart_audio(&mut self, reason: &str) {
        crate::log!("supervisor: audio restart ({reason})");
        if let Some(leg) = self.audio.take() {
            leg.teardown();
        }
        self.audio_retry_at = Some(Instant::now());
    }

    fn save(&mut self) {
        if self.save_in_flight.load(Ordering::Relaxed) {
            self.pending_save = true;
            return;
        }
        let Some(leg) = &self.video else {
            play_feedback(false);
            self.tray.notify("Instant Replay", "Capture is not running", true);
            return;
        };
        let snapshot = match self.ring.lock() {
            Ok(r) => r.snapshot(),
            Err(_) => return,
        };
        let clip_secs = self.runtime.clip_seconds();
        let prm = mux::MuxParams {
            width: leg.out_w,
            height: leg.out_h,
            fps: leg.fps,
            bitrate: leg.bitrate,
            seq_header: leg.enc.meta.lock().ok().and_then(|m| m.seq_header.clone()),
            audio: self
                .audio
                .as_ref()
                .and_then(|a| a.meta.lock().ok().and_then(|m| m.clone())),
        };
        let dir = self.runtime.clips_dir();
        let (tray, flag) = (self.tray.clone(), self.save_in_flight.clone());
        flag.store(true, Ordering::Relaxed);
        let _ = std::thread::Builder::new().name("writer".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            let started = Instant::now();
            let path = dir.join(mux::clip_filename());
            let r = (|| -> std::result::Result<(f64, u64), String> {
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let clip = crate::ring::trim(&snapshot, clip_secs).ok_or("buffer is empty")?;
                mux::save_clip(&path, &clip, &prm).map_err(|e| e.to_string())?;
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                Ok((clip.duration as f64 / 1e7, size))
            })();
            match r {
                Ok((secs, size)) => {
                    crate::log!(
                        "save: {} ({:.1} s, {:.1} MB) in {} ms",
                        path.display(),
                        secs,
                        size as f64 / 1e6,
                        started.elapsed().as_millis()
                    );
                    play_feedback(true);
                    tray.notify("Clip saved", &format!("{:.0} s — {}", secs, path.display()), false);
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&path);
                    crate::log!("save: failed: {e}");
                    play_feedback(false);
                    tray.notify("Clip failed", &e, true);
                }
            }
            flag.store(false, Ordering::Relaxed);
        });
    }

    /// WGC only delivers on screen change, so a silent encoder is normal.
    /// Probe the device before declaring the leg dead.
    fn watchdog(&mut self) {
        let Some(leg) = &self.video else { return };
        let last = leg.last_output.load(Ordering::Relaxed);
        if last == 0 {
            // No output ever: give the leg 30 s from build before judging.
            if leg.up_since.elapsed() > Duration::from_secs(30) {
                let removed = unsafe { leg.d3d.device.GetDeviceRemovedReason() };
                crate::log!("supervisor: watchdog — no output since start, device: {removed:?}");
                self.restart_video("watchdog: no output since start");
            }
            return;
        }
        let now = crate::frames::qpc_now();
        if now - last > 30 * crate::ring::SEC {
            let device_ok = unsafe { leg.d3d.device.GetDeviceRemovedReason().is_ok() };
            let source_ok = leg.cap.probe();
            if !device_ok || !source_ok {
                self.restart_video("watchdog: device probe failed");
            } else {
                // Healthy, screen just static: back off the next probe.
                leg.last_output.store(now - 25 * crate::ring::SEC, Ordering::Relaxed);
            }
        }
    }

    fn tick(&mut self) {
        if self.video.is_none() {
            if let Some(at) = self.video_retry_at {
                if Instant::now() >= at {
                    self.try_build_video();
                }
            }
        } else {
            self.watchdog();
            // Reset restart backoff after a healthy minute.
            if self.video_backoff > 0
                && self.video.as_ref().is_some_and(|l| l.up_since.elapsed() > Duration::from_secs(60))
            {
                self.video_backoff = 0;
            }
        }
        if self.audio.is_none() {
            if let Some(at) = self.audio_retry_at {
                if Instant::now() >= at {
                    // AudioLeg::start never fails synchronously; failures arrive as AudioLegDied.
                    self.audio = Some(crate::audio::AudioLeg::start(
                        self.ring.clone(),
                        self.tx.clone(),
                        self.runtime.mic(),
                    ));
                    self.audio_retry_at = None;
                    self.audio_up_since = Some(Instant::now());
                }
            }
        } else if self.audio_backoff > 0
            && self.audio_up_since.is_some_and(|t| t.elapsed() > Duration::from_secs(60))
        {
            self.audio_backoff = 0;
        }
        if self.pending_save && !self.save_in_flight.load(Ordering::Relaxed) {
            self.pending_save = false;
            self.save();
        }
    }
}

pub fn run(rx: Receiver<Ctl>, runtime: Arc<RuntimeConfig>, tray: TrayHandle, tx: Sender<Ctl>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let ring = Arc::new(Mutex::new(Ring::new(runtime.clip_seconds(), RING_BYTE_CAP)));
    let notifier = crate::audio::DeviceNotifier::register(tx.clone());
    if let Err(e) = &notifier {
        crate::log!("supervisor: device notifier failed: {e}");
    }
    let mut s = Supervisor {
        runtime,
        tray,
        tx,
        ring,
        video: None,
        video_backoff: 0,
        video_retry_at: Some(Instant::now()),
        audio: None,
        audio_backoff: 0,
        audio_retry_at: Some(Instant::now()),
        audio_up_since: None,
        save_in_flight: Arc::new(AtomicBool::new(false)),
        pending_save: false,
    };
    crate::log!("supervisor: started");
    s.tick();

    loop {
        let msg = rx.recv_timeout(Duration::from_millis(500));
        if matches!(msg, Ok(Ctl::Quit) | Err(RecvTimeoutError::Disconnected)) {
            break;
        }
        // A panic in any handler must not kill the supervisor.
        let r = catch_unwind(AssertUnwindSafe(|| {
            match msg {
                Ok(Ctl::SaveClip) => s.save(),
                Ok(Ctl::SetClipLen(n)) => {
                    if let Ok(mut r) = s.ring.lock() {
                        r.set_clip_seconds(n);
                    }
                    crate::log!("supervisor: clip length {n} s");
                }
                Ok(Ctl::CursorChanged(on)) => {
                    if let Some(leg) = &s.video {
                        leg.cap.set_cursor(on);
                    }
                }
                Ok(Ctl::VideoRestart(reason)) => s.restart_video(reason),
                Ok(Ctl::VideoLegDied(e)) => {
                    crate::log!("supervisor: video leg died: {e}");
                    s.restart_video("leg died");
                }
                Ok(Ctl::AudioRestart(reason)) => s.restart_audio(reason),
                Ok(Ctl::AudioLegDied(e)) => {
                    crate::log!("supervisor: audio leg died: {e}");
                    if let Some(leg) = s.audio.take() {
                        leg.teardown();
                    }
                    let delay = BACKOFF[s.audio_backoff.min(BACKOFF.len() - 1)];
                    s.audio_backoff += 1;
                    s.audio_retry_at = Some(Instant::now() + delay);
                }
                Ok(Ctl::Quit) | Err(_) => {}
            }
            s.tick();
        }));
        if r.is_err() {
            crate::log!("supervisor: handler panicked (continuing)");
        }
    }

    if let Some(leg) = s.video.take() {
        leg.teardown();
    }
    if let Some(leg) = s.audio.take() {
        leg.teardown();
    }
    if let Ok((enumerator, client)) = notifier {
        unsafe {
            let _ = enumerator.UnregisterEndpointNotificationCallback(&client);
        }
    }
    // Bounded wait for an in-flight save.
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.save_in_flight.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    crate::log!("supervisor: stopped");
}
