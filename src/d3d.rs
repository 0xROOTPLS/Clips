//! D3D11 device on the adapter that owns the primary monitor, plus the
//! WinRT device wrapper and MF DXGI device manager shared by the video leg.

use windows::core::{Interface, Result};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

pub struct D3D {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub winrt_device: IDirect3DDevice,
    pub mf_manager: IMFDXGIDeviceManager,
    pub mf_token: u32,
    pub hmonitor: isize,
}

pub fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

pub struct MonitorInfo {
    pub index: u32, // 1-based, DXGI enumeration order
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    pub hmonitor: isize,
}

/// All attached outputs across adapters, in stable DXGI order.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let mut out = Vec::new();
    let primary = primary_monitor();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.AttachedToDesktop.as_bool() {
                        let r = desc.DesktopCoordinates;
                        out.push(MonitorInfo {
                            index: out.len() as u32 + 1,
                            name: String::from_utf16_lossy(&desc.DeviceName)
                                .trim_end_matches('\0')
                                .to_string(),
                            width: (r.right - r.left).unsigned_abs(),
                            height: (r.bottom - r.top).unsigned_abs(),
                            primary: desc.Monitor == primary,
                            hmonitor: desc.Monitor.0 as isize,
                        });
                    }
                }
                j += 1;
            }
            i += 1;
        }
    }
    out
}

/// `selector`: 0 = primary, n = nth monitor from `list_monitors`.
/// Falls back to primary if the selected monitor is gone.
fn resolve_monitor(selector: u32) -> HMONITOR {
    if selector > 0 {
        if let Some(m) = list_monitors().into_iter().find(|m| m.index == selector) {
            return HMONITOR(m.hmonitor as *mut _);
        }
        crate::log!("d3d: monitor {selector} not found, using primary");
    }
    primary_monitor()
}

fn adapter_for_monitor(hmonitor: HMONITOR) -> Result<Option<IDXGIAdapter1>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let mut j = 0;
            while let Ok(output) = adapter.EnumOutputs(j) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor == hmonitor {
                        return Ok(Some(adapter));
                    }
                }
                j += 1;
            }
            i += 1;
        }
        Ok(None)
    }
}

pub fn create_for_primary() -> Result<D3D> {
    create_for_monitor(0)
}

pub fn create_for_monitor(selector: u32) -> Result<D3D> {
    unsafe {
        let hmonitor = resolve_monitor(selector);
        let adapter: Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter> =
            adapter_for_monitor(hmonitor)?.and_then(|a| a.cast().ok());

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let driver_type = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE
        };
        D3D11CreateDevice(
            adapter.as_ref(),
            driver_type,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.unwrap();
        let context = context.unwrap();

        // Capture pool thread, encoder, and VP all touch this device.
        device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);

        let mut mf_token = 0u32;
        let mut mf_manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut mf_token, &mut mf_manager)?;
        let mf_manager = mf_manager.unwrap();
        mf_manager.ResetDevice(&device, mf_token)?;

        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;

        if let Ok(desc) = dxgi.GetAdapter().and_then(|a| a.GetDesc()) {
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            crate::log!("d3d: device on '{}'", name);
        }

        Ok(D3D {
            device,
            context,
            winrt_device,
            mf_manager,
            mf_token,
            hmonitor: hmonitor.0 as isize,
        })
    }
}
