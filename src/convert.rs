//! GPU BGRA→NV12 conversion (+ optional downscale) via ID3D11VideoProcessor.

use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_BIND_RENDER_TARGET,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::d3d::D3D;

const POOL: usize = 16; // > AMF's internal queue depth (~4-8)

pub struct Converter {
    video_context: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    vp_enum: ID3D11VideoProcessorEnumerator,
    pool: Vec<ID3D11Texture2D>,
    next: usize,
    pub out_w: u32,
    pub out_h: u32,
}

// SAFETY: only used from the WGC callback thread; device is multithread-protected.
unsafe impl Send for Converter {}

/// Output size: `target_h` 0 (native) or >= input passes through; otherwise
/// aspect-preserving downscale to `target_h` high. Dimensions forced even.
pub fn output_size(in_w: u32, in_h: u32, target_h: u32) -> (u32, u32) {
    if target_h == 0 || in_h <= target_h {
        (in_w & !1, in_h & !1)
    } else {
        let w = (in_w as u64 * target_h as u64 / in_h as u64) as u32;
        (w & !1, target_h & !1)
    }
}

impl Converter {
    pub fn new(d3d: &D3D, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Converter> {
        unsafe {
            let video_device: ID3D11VideoDevice = d3d.device.cast()?;
            let video_context: ID3D11VideoContext = d3d.context.cast()?;

            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let vp_enum = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let vp = video_device.CreateVideoProcessor(&vp_enum, 0)?;

            if let Ok(vc1) = video_context.cast::<ID3D11VideoContext1>() {
                vc1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                vc1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut pool = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let mut t: Option<ID3D11Texture2D> = None;
                d3d.device.CreateTexture2D(&tex_desc, None, Some(&mut t))?;
                pool.push(t.unwrap());
            }

            Ok(Converter { video_context, vp, vp_enum, pool, next: 0, out_w, out_h })
        }
    }

    /// Blt `src` into the next pooled NV12 texture and return it.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        unsafe {
            let dst = self.pool[self.next].clone();
            self.next = (self.next + 1) % self.pool.len();

            let video_device: ID3D11VideoDevice = src.GetDevice()?.cast()?;

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: Default::default(),
            };
            let mut in_view = None;
            video_device.CreateVideoProcessorInputView(src, &self.vp_enum, &in_desc, Some(&mut in_view))?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: Default::default(),
            };
            let mut out_view = None;
            video_device.CreateVideoProcessorOutputView(&dst, &self.vp_enum, &out_desc, Some(&mut out_view))?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(in_view),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.video_context.VideoProcessorBlt(&self.vp, out_view.as_ref().unwrap(), 0, &streams);
            for mut s in streams {
                std::mem::ManuallyDrop::drop(&mut s.pInputSurface);
            }
            r?;
            Ok(dst)
        }
    }
}
