//! Windows.Graphics.Capture of the primary monitor: free-threaded frame pool,
//! frames delivered to a caller-supplied callback on the WGC pool thread.

use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::d3d::D3D;

pub struct Capture {
    pub item: GraphicsCaptureItem,
    pub pool: Direct3D11CaptureFramePool,
    pub session: GraphicsCaptureSession,
    pub size: SizeInt32,
}

/// True when the WGC yellow border can be disabled (IsBorderRequired exists
/// on Win11 only). False on Win10 → caller should use the DXGI fallback.
pub fn border_removable() -> bool {
    use windows::core::HSTRING;
    windows::Foundation::Metadata::ApiInformation::IsPropertyPresent(
        &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
        &HSTRING::from("IsBorderRequired"),
    )
    .unwrap_or(false)
}

/// Capture item for the monitor the D3D device was built for.
pub fn create_item(d3d: &D3D) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForMonitor(HMONITOR(d3d.hmonitor as *mut _)) }
}

impl Capture {
    pub fn start<F, G>(d3d: &D3D, item: GraphicsCaptureItem, cursor: bool, on_frame: F, on_closed: G) -> Result<Capture>
    where
        F: FnMut(&Direct3D11CaptureFrame) + Send + 'static,
        G: Fn() + Send + 'static,
    {
        // TypedEventHandler wants Fn; WGC serializes callbacks, so a Mutex is uncontended.
        let on_frame = std::sync::Mutex::new(on_frame);
        let size = item.Size()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d.winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;

        pool.FrameArrived(&TypedEventHandler::new(
            move |pool: windows_core::Ref<Direct3D11CaptureFramePool>, _| {
                if let Some(pool) = pool.as_ref() {
                    if let Ok(frame) = pool.TryGetNextFrame() {
                        if let Ok(mut f) = on_frame.lock() {
                            f(&frame);
                        }
                        let _ = frame.Close();
                    }
                }
                Ok(())
            },
        ))?;

        item.Closed(&TypedEventHandler::new(move |_, _| {
            on_closed();
            Ok(())
        }))?;

        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        // Yellow-border opt-out: unpackaged apps must request Borderless access
        // first (auto-granted on Win11; the API doesn't exist on Win10).
        match windows::Graphics::Capture::GraphicsCaptureAccess::RequestAccessAsync(
            windows::Graphics::Capture::GraphicsCaptureAccessKind::Borderless,
        )
        .and_then(|op| op.join())
        {
            Ok(status) => crate::log!("capture: borderless access: {status:?}"),
            Err(e) => crate::log!("capture: borderless access unavailable (Win10?): {e}"),
        }
        if let Err(e) = session.SetIsBorderRequired(false) {
            crate::log!("capture: SetIsBorderRequired failed (yellow border stays): {e}");
        }
        session.StartCapture()?;
        crate::log!("capture: started {}x{}", size.Width, size.Height);

        Ok(Capture { item, pool, session, size })
    }

    pub fn set_cursor(&self, enabled: bool) {
        let _ = self.session.SetIsCursorCaptureEnabled(enabled);
    }

    pub fn stop(&self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Texture behind a capture frame plus its QPC-relative PTS (100 ns).
pub fn frame_texture(
    frame: &Direct3D11CaptureFrame,
) -> Result<(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, i64)> {
    use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    let texture = unsafe { access.GetInterface()? };
    let pts = frame.SystemRelativeTime()?.Duration;
    Ok((texture, pts))
}
