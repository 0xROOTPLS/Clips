//! Async hardware HEVC MFT (AMD AMF). Output-type-first negotiation,
//! D3D11 texture input, event-driven encode loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

use crate::d3d::D3D;
use crate::frames::FrameQueue;

pub struct EncodedFrame {
    pub data: Box<[u8]>,
    pub pts: i64,
    pub dur: i64,
    pub keyframe: bool,
}

#[derive(Default, Clone)]
pub struct VideoMeta {
    pub seq_header: Option<Vec<u8>>, // VPS/SPS/PPS from MF_MT_MPEG_SEQUENCE_HEADER
}

pub struct Encoder {
    mft: IMFTransform,
    event_gen: IMFMediaEventGenerator,
    pub meta: Arc<Mutex<VideoMeta>>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    frame_dur: i64,
}

// SAFETY: MF objects are free-threaded; wake/shutdown race only with GetEvent.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_bool(b: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: windows::Win32::Foundation::VARIANT_BOOL(if b { -1 } else { 0 }),
                },
            }),
        },
    }
}

fn set_codec_var(api: &ICodecAPI, guid: &GUID, v: VARIANT, name: &str) {
    unsafe {
        if let Err(e) = api.SetValue(guid, &v) {
            crate::log!("encoder: codecapi {name} rejected: {e}");
        }
    }
}

impl Encoder {
    pub fn new(d3d: &D3D, width: u32, height: u32, fps: u32, bitrate: u32, gop_frames: u32) -> Result<Encoder> {
        unsafe {
            let reg = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&reg),
                &mut activates,
                &mut count,
            )?;
            if count == 0 || activates.is_null() {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "no hardware HEVC encoder found",
                ));
            }
            let list = std::slice::from_raw_parts_mut(activates, count as usize);
            let activate = list[0].take().unwrap();
            let mut pw = windows_core::PWSTR::null();
            let mut pw_len = 0u32;
            if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut pw_len).is_ok() && !pw.is_null() {
                crate::log!("encoder: using '{}'", String::from_utf16_lossy(pw.as_wide()));
                windows::Win32::System::Com::CoTaskMemFree(Some(pw.as_ptr() as *const _));
            }
            let mft: IMFTransform = activate.ActivateObject()?;
            for a in list.iter_mut() {
                drop(a.take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 1 {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "encoder MFT is not async",
                ));
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;

            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, d3d.mf_manager.as_raw() as usize)?;

            // Async hardware encoders require output type before input type.
            let out_t = MFCreateMediaType()?;
            out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
            out_t.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            out_t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            out_t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out_t.SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
            mft.SetOutputType(0, &out_t, 0)?;

            let in_t = MFCreateMediaType()?;
            in_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_t.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            in_t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
            in_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &in_t, 0)?;

            if let Ok(api) = mft.cast::<ICodecAPI>() {
                set_codec_var(&api, &CODECAPI_AVEncCommonRateControlMode,
                    variant_u32(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32), "rate_control");
                set_codec_var(&api, &CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate), "mean_bitrate");
                set_codec_var(&api, &CODECAPI_AVEncCommonMaxBitRate, variant_u32(bitrate / 3 * 5), "max_bitrate");
                set_codec_var(&api, &CODECAPI_AVEncMPVGOPSize, variant_u32(gop_frames), "gop_size");
                set_codec_var(&api, &CODECAPI_AVLowLatencyMode, variant_bool(true), "low_latency");
                set_codec_var(&api, &CODECAPI_AVEncCommonQualityVsSpeed, variant_u32(70), "quality_vs_speed");
            }

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            // First muxable sample within one frame instead of one GOP.
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                set_codec_var(&api, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1), "force_keyframe");
            }

            let event_gen: IMFMediaEventGenerator = mft.cast()?;
            Ok(Encoder {
                mft,
                event_gen,
                meta: Arc::new(Mutex::new(VideoMeta::default())),
                width,
                height,
                fps,
                bitrate,
                frame_dur: 10_000_000 / fps.max(1) as i64,
            })
        }
    }

    /// Blocking encode loop; returns on `stop` or fatal error.
    pub fn run<F: FnMut(EncodedFrame)>(
        &self,
        queue: &FrameQueue,
        stop: &AtomicBool,
        mut sink: F,
    ) -> Result<()> {
        unsafe {
            loop {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let ev = self.event_gen.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))?;
                let ty = ev.GetType()? as i32;
                match MF_EVENT_TYPE(ty) {
                    METransformNeedInput => {
                        loop {
                            if stop.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            if let Some(f) = queue.pop_timeout(Duration::from_millis(100)) {
                                self.feed(&f.tex, f.pts)?;
                                break;
                            }
                        }
                    }
                    METransformHaveOutput => self.drain_output(&mut sink)?,
                    MEError => {
                        let hr = ev.GetStatus().unwrap_or(S_OK);
                        return Err(windows::core::Error::new(hr, "encoder MEError"));
                    }
                    _ => {}
                }
            }
        }
    }

    /// Unblock a thread sitting in `run` (pair with setting `stop`).
    pub fn wake(&self, queue: &FrameQueue) {
        queue.notify();
        unsafe {
            let _ = self.event_gen.QueueEvent(MEUnknown.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null());
        }
    }

    pub fn shutdown(&self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }

    fn feed(&self, tex: &ID3D11Texture2D, pts: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(self.frame_dur)?;
            self.mft.ProcessInput(0, &sample, 0)
        }
    }

    fn drain_output<F: FnMut(EncodedFrame)>(&self, sink: &mut F) -> Result<()> {
        unsafe {
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER::default()];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut bufs, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
            std::mem::ManuallyDrop::drop(&mut bufs[0].pEvents);
            match r {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let pts = sample.GetSampleTime().unwrap_or(0);
                        let dur = sample.GetSampleDuration().unwrap_or(self.frame_dur);
                        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
                        let buf = sample.ConvertToContiguousBuffer()?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        buf.Lock(&mut ptr, None, Some(&mut len))?;
                        let data: Box<[u8]> = std::slice::from_raw_parts(ptr, len as usize).into();
                        let _ = buf.Unlock();
                        sink(EncodedFrame { data, pts, dur, keyframe });
                    }
                    Ok(())
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    let mut blob_ptr = std::ptr::null_mut();
                    let mut blob_len = 0u32;
                    if t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob_ptr, &mut blob_len).is_ok()
                        && !blob_ptr.is_null()
                    {
                        let hdr = std::slice::from_raw_parts(blob_ptr, blob_len as usize).to_vec();
                        windows::Win32::System::Com::CoTaskMemFree(Some(blob_ptr as *const _));
                        crate::log!("encoder: sequence header {} bytes", hdr.len());
                        if let Ok(mut m) = self.meta.lock() {
                            m.seq_header = Some(hdr);
                        }
                    } else {
                        crate::log!("encoder: stream change without sequence header");
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    }
}
