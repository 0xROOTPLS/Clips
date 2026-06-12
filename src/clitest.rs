//! CLI test harnesses: --capture-test, --encode-test, --record-test.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{MFStartup, MF_VERSION, MFSTARTUP_FULL};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::encoder::EncodedFrame;
use crate::frames::{FrameQueue, Pacer, QFrame};
use crate::{capture, convert, d3d, encoder, log};

use windows::core::PCWSTR;
use windows::Win32::Media::MediaFoundation::{
    IMFSample, MFCreateSourceReaderFromURL, MF_PD_DURATION, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE,
};

fn init_mta() -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            log!("test: MFStartup failed: {e}");
            return false;
        }
    }
    true
}

/// Count WGC frames for `secs`; exit 0 if any arrived.
pub fn capture_test(secs: u64) -> i32 {
    if !init_mta() {
        return 1;
    }
    let d3d = match d3d::create_for_primary() {
        Ok(d) => d,
        Err(e) => {
            log!("capture-test: d3d failed: {e}");
            return 1;
        }
    };
    let item = match capture::create_item(&d3d) {
        Ok(i) => i,
        Err(e) => {
            log!("capture-test: item failed: {e}");
            return 1;
        }
    };
    let frames = Arc::new(AtomicU64::new(0));
    let f2 = frames.clone();
    let cap = match capture::Capture::start(
        &d3d,
        item,
        true,
        move |_| {
            f2.fetch_add(1, Ordering::Relaxed);
        },
        || log!("capture-test: item closed"),
    ) {
        Ok(c) => c,
        Err(e) => {
            log!("capture-test: start failed: {e}");
            return 1;
        }
    };
    std::thread::sleep(Duration::from_secs(secs));
    cap.stop();
    let n = frames.load(Ordering::Relaxed);
    log!("capture-test: {} frames in {} s ({:.1} fps)", n, secs, n as f64 / secs as f64);
    (n == 0) as i32 * 2
}

/// Full pipeline (video + audio legs → ring → trim → mux) for `secs`,
/// then re-open the MP4 and validate structure. CI-able smoke test.
pub fn record_test(secs: u64, backend: Option<String>) -> i32 {
    if !init_mta() {
        return 1;
    }
    let path = std::env::temp_dir().join("clips-record-test.mp4");
    let _ = std::fs::remove_file(&path);

    let mut cfg = crate::config::Config::default();
    cfg.clip_seconds = 60; // window must cover the whole test
    if let Some(b) = backend {
        cfg.backend = b;
    }
    let runtime = Arc::new(crate::config::RuntimeConfig::from(&cfg));
    let (tx, rx) = std::sync::mpsc::channel();
    let ring = Arc::new(Mutex::new(crate::ring::Ring::new(60, 400 * 1024 * 1024)));

    let video = match crate::supervisor::build_video_leg(&runtime, &ring, &tx) {
        Ok(v) => v,
        Err(e) => {
            log!("record-test: video leg failed: {e}");
            return 1;
        }
    };
    let audio = crate::audio::AudioLeg::start(ring.clone(), tx.clone(), "default".into());

    std::thread::sleep(Duration::from_secs(secs));

    let mut leg_errors = 0;
    while let Ok(msg) = rx.try_recv() {
        log!("record-test: ctl during run: {msg:?}");
        if matches!(msg, crate::supervisor::Ctl::VideoLegDied(_) | crate::supervisor::Ctl::AudioLegDied(_)) {
            leg_errors += 1;
        }
    }

    let snapshot = ring.lock().unwrap().snapshot();
    let prm = video.mux_params(audio.meta.lock().ok().and_then(|m| m.clone()));
    video.teardown();
    audio.teardown();

    let Some(clip) = crate::ring::trim(&snapshot, secs as u32) else {
        log!("record-test: no usable video in ring");
        return 2;
    };
    if let Err(e) = crate::mux::save_clip(&path, &clip, &prm) {
        log!("record-test: mux failed: {e}");
        return 2;
    }

    match validate_mp4(&path, secs, prm.audio.is_some()) {
        Ok(()) if leg_errors == 0 => {
            log!("record-test: PASS ({})", path.display());
            0
        }
        Ok(()) => {
            log!("record-test: mp4 ok but {leg_errors} leg error(s) during run");
            3
        }
        Err(e) => {
            log!("record-test: validation failed: {e}");
            2
        }
    }
}

fn validate_mp4(path: &std::path::Path, expect_secs: u64, expect_audio: bool) -> windows::core::Result<()> {
    unsafe {
        let w: Vec<u16> = path.display().to_string().encode_utf16().chain(std::iter::once(0)).collect();
        let reader = MFCreateSourceReaderFromURL(PCWSTR(w.as_ptr()), None)?;

        let v_ok = reader.GetNativeMediaType(0, 0).is_ok();
        let a_ok = reader.GetNativeMediaType(1, 0).is_ok();
        log!("record-test: streams video={v_ok} second={a_ok}");
        if !v_ok || (expect_audio && !a_ok) {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "missing stream"));
        }

        let pv = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)?;
        let dur_100ns = pv.Anonymous.Anonymous.Anonymous.uhVal as i64;
        let dur_s = dur_100ns as f64 / 1e7;
        log!("record-test: duration {dur_s:.2} s (expected ~{expect_secs})");
        // Trim granularity is one GOP (2 s) plus startup latency.
        if dur_s < expect_secs as f64 - 3.5 || dur_s > expect_secs as f64 + 2.5 {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "duration out of range"));
        }

        // First video sample must be readable and a sync point.
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            0,
            None,
            Some(&mut flags),
            None,
            Some(&mut sample),
        )?;
        let sample = sample.ok_or_else(|| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "no first video sample")
        })?;
        let len = sample.GetTotalLength()?;
        if len == 0 {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "empty first sample"));
        }
        Ok(())
    }
}

/// Full capture→convert→encode chain for `secs`; dumps Annex-B HEVC to test.hevc.
pub fn encode_test(secs: u64) -> i32 {
    if !init_mta() {
        return 1;
    }
    let r = (|| -> windows::core::Result<(usize, usize, usize, i64)> {
        let d3d = d3d::create_for_primary()?;
        let item = capture::create_item(&d3d)?;
        let size = item.Size()?;
        let (in_w, in_h) = (size.Width as u32, size.Height as u32);
        let (out_w, out_h) = convert::output_size(in_w, in_h, 1080);
        log!("encode-test: {}x{} -> {}x{}", in_w, in_h, out_w, out_h);

        let mut converter = convert::Converter::new(&d3d, in_w, in_h, out_w, out_h, 60)?;
        let queue = Arc::new(FrameQueue::default());
        let enc = Arc::new(encoder::Encoder::new(&d3d, out_w, out_h, 60, 15_000_000, 120)?);
        let stop = Arc::new(AtomicBool::new(false));

        let q2 = queue.clone();
        let mut pacer = Pacer::new(60);
        let cap = capture::Capture::start(
            &d3d,
            item,
            true,
            move |frame| {
                if let Ok((tex, pts)) = capture::frame_texture(frame) {
                    if let Some(snapped) = pacer.accept(pts) {
                        if let Ok(nv12) = converter.convert(&tex) {
                            q2.push(QFrame { tex: nv12, pts: snapped });
                        }
                    }
                }
            },
            || log!("encode-test: item closed"),
        )?;

        let out: Arc<Mutex<Vec<EncodedFrame>>> = Arc::new(Mutex::new(Vec::new()));
        let (enc2, q3, stop2, out2) = (enc.clone(), queue.clone(), stop.clone(), out.clone());
        let th = std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || {
                if let Err(e) = enc2.run(&q3, &stop2, |f| out2.lock().unwrap().push(f)) {
                    log!("encode-test: encoder error: {e}");
                }
            })
            .unwrap();

        std::thread::sleep(Duration::from_secs(secs));
        cap.stop();
        stop.store(true, Ordering::Relaxed);
        enc.wake(&queue);
        let _ = th.join();
        enc.shutdown();

        let frames = out.lock().unwrap();
        let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        let keys = frames.iter().filter(|f| f.keyframe).count();
        let span = match (frames.first(), frames.last()) {
            (Some(a), Some(b)) => b.pts - a.pts + b.dur,
            _ => 0,
        };
        let mut blob = Vec::with_capacity(bytes);
        for f in frames.iter() {
            blob.extend_from_slice(&f.data);
        }
        std::fs::write("test.hevc", &blob).ok();
        let seq = enc.meta.lock().unwrap().seq_header.clone();
        log!(
            "encode-test: {} samples, {} keyframes, {} KB, span {:.2} s, seq_header {:?} bytes, start code ok: {}",
            frames.len(),
            keys,
            bytes / 1024,
            span as f64 / 1e7,
            seq.map(|s| s.len()),
            blob.starts_with(&[0, 0, 0, 1]) || blob.starts_with(&[0, 0, 1])
        );
        Ok((frames.len(), keys, bytes, span))
    })();
    match r {
        Ok((n, keys, bytes, _)) if n > 0 && keys > 0 && bytes > 0 => 0,
        Ok(_) => {
            log!("encode-test: produced no usable output");
            2
        }
        Err(e) => {
            log!("encode-test: failed: {e}");
            1
        }
    }
}
