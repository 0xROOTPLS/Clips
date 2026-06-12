//! DXGI Desktop Duplication capture: fallback for Win10, where the WGC
//! yellow border cannot be disabled. Same delivery model as WGC (frames
//! only on screen change, QPC timestamps); cursor is not composited.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::d3d::D3D;

pub struct DuplCapture {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Output on the device's adapter matching the capture monitor, plus desktop size.
pub fn output_for(d3d: &D3D) -> Result<(IDXGIOutput1, u32, u32)> {
    unsafe {
        let adapter = d3d.device.cast::<IDXGIDevice>()?.GetAdapter()?;
        let mut j = 0;
        while let Ok(output) = adapter.EnumOutputs(j) {
            if let Ok(desc) = output.GetDesc() {
                if desc.Monitor.0 as isize == d3d.hmonitor && desc.AttachedToDesktop.as_bool() {
                    let r = desc.DesktopCoordinates;
                    return Ok((
                        output.cast()?,
                        (r.right - r.left).unsigned_abs(),
                        (r.bottom - r.top).unsigned_abs(),
                    ));
                }
            }
            j += 1;
        }
        Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "no DXGI output for capture monitor",
        ))
    }
}

fn qpc_freq() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut f);
    }
    f.max(1)
}

/// Retry DuplicateOutput until it succeeds; secure desktop (lock screen, UAC)
/// holds the output indefinitely. None => mode size changed or stopping —
/// caller must exit so the supervisor rebuilds the leg.
fn reacquire(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    w: u32,
    h: u32,
    stop: &AtomicBool,
) -> Option<IDXGIOutputDuplication> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        match unsafe { output.DuplicateOutput(device) } {
            Ok(d) => {
                let desc = unsafe { d.GetDesc() };
                if (desc.ModeDesc.Width, desc.ModeDesc.Height) != (w, h) {
                    crate::log!(
                        "dupl: mode changed {}x{} -> {}x{}",
                        w,
                        h,
                        desc.ModeDesc.Width,
                        desc.ModeDesc.Height
                    );
                    return None;
                }
                crate::log!("dupl: reacquired");
                return Some(d);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

impl DuplCapture {
    pub fn start<F, G>(
        d3d: &D3D,
        output: IDXGIOutput1,
        w: u32,
        h: u32,
        fps: u32,
        mut on_frame: F,
        on_closed: G,
    ) -> Result<DuplCapture>
    where
        F: FnMut(&ID3D11Texture2D, i64) + Send + 'static,
        G: Fn() + Send + 'static,
    {
        let dupl = unsafe { output.DuplicateOutput(&d3d.device)? };
        let desc = unsafe { dupl.GetDesc() };
        if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation.0 != 0 {
            crate::log!("dupl: rotated output ({:?}) — frames arrive unrotated", desc.Rotation);
        }

        let freq = qpc_freq();
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let device = d3d.device.clone();
        let (stop2, alive2) = (stop.clone(), alive.clone());

        // Acquire pacing: image frames at ~4/3 fps (pacer thins to exact fps),
        // cursor-only updates coalesced (1000 Hz mice fire one per poll).
        let frame_pause = Duration::from_micros(750_000 / fps.max(1) as u64);
        let cursor_pause = Duration::from_millis(4);

        let thread = std::thread::Builder::new()
            .name("dupl".into())
            .spawn(move || {
                let mut dupl = dupl;
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        break;
                    }
                    let started = std::time::Instant::now();
                    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                    let mut resource: Option<IDXGIResource> = None;
                    match unsafe { dupl.AcquireNextFrame(100, &mut info, &mut resource) } {
                        Ok(()) => {
                            // LastPresentTime == 0: cursor-only update, no new desktop image.
                            let has_image = info.LastPresentTime != 0;
                            if has_image {
                                if let Some(tex) =
                                    resource.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok())
                                {
                                    // Raw QPC ticks -> 100 ns; same clock as WGC/WASAPI PTS.
                                    let pts =
                                        (info.LastPresentTime as i128 * 10_000_000 / freq as i128) as i64;
                                    on_frame(&tex, pts);
                                }
                            }
                            drop(resource);
                            let _ = unsafe { dupl.ReleaseFrame() };
                            // Release first, then pause: DDA coalesces, the next
                            // acquire returns the newest image. Caps wakeups and
                            // DWM copy churn instead of spinning at refresh/poll rate.
                            let pause = if has_image { frame_pause } else { cursor_pause };
                            if let Some(rest) = pause.checked_sub(started.elapsed()) {
                                std::thread::sleep(rest);
                            }
                        }
                        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                        Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                            crate::log!("dupl: access lost, reacquiring");
                            match reacquire(&output, &device, w, h, &stop2) {
                                Some(d) => dupl = d,
                                None => {
                                    if !stop2.load(Ordering::Relaxed) {
                                        on_closed();
                                    }
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            crate::log!("dupl: AcquireNextFrame failed: {e}");
                            on_closed();
                            break;
                        }
                    }
                }
                alive2.store(false, Ordering::Relaxed);
            })
            .expect("spawn dupl");

        crate::log!("dupl: started {}x{} (DXGI desktop duplication)", w, h);
        Ok(DuplCapture { stop, alive, thread: Some(thread) })
    }

    /// Watchdog probe: worker thread still running.
    pub fn probe(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DuplCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
