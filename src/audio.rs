//! System-audio leg: WASAPI loopback → silence-gap filling → AAC MFT → ring.
//! PTS clock is the shared QPC (100 ns), so A/V sync needs no extra work.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{implement, Result, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, ERole, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::mux::AudioParams;
use crate::ring::{Packet, Ring, StreamKind};
use crate::supervisor::Ctl;

const SEC: i64 = 10_000_000;
const AAC_BYTES_PER_SEC: u32 = 24_000; // 192 kbps
const SILENCE_MARGIN: i64 = 50 * SEC / 1000; // stay 50 ms behind 'now' when synthesizing

pub struct AudioLeg {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub meta: Arc<Mutex<Option<AudioParams>>>,
}

impl AudioLeg {
    pub fn start(ring: Arc<Mutex<Ring>>, tx: Sender<Ctl>, mic_sel: String) -> AudioLeg {
        let stop = Arc::new(AtomicBool::new(false));
        let meta = Arc::new(Mutex::new(None));
        let (stop2, meta2) = (stop.clone(), meta.clone());
        let thread = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let r = catch_unwind(AssertUnwindSafe(|| run_audio(&ring, &stop2, &meta2, &mic_sel)));
                match r {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        let _ = tx.send(Ctl::AudioLegDied(format!("{e}")));
                    }
                    Err(_) => {
                        let _ = tx.send(Ctl::AudioLegDied("audio thread panicked".into()));
                    }
                }
            })
            .expect("spawn audio");
        AudioLeg { stop, thread: Some(thread), meta }
    }

    pub fn teardown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        crate::log!("audio leg: torn down");
    }
}

/// Notifies the supervisor when the default render device changes.
#[implement(IMMNotificationClient)]
pub struct DeviceNotifier {
    tx: Sender<Ctl>,
}

impl DeviceNotifier {
    pub fn register(tx: Sender<Ctl>) -> Result<(IMMDeviceEnumerator, IMMNotificationClient)> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client: IMMNotificationClient = DeviceNotifier { tx }.into();
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            Ok((enumerator, client))
        }
    }
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: windows::Win32::Media::Audio::DEVICE_STATE) -> Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> Result<()> {
        if role == eConsole && (flow == eRender || flow == eCapture) {
            let _ = self.tx.send(Ctl::AudioRestart("default audio device changed"));
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &windows::Win32::Foundation::PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

struct MixFormat {
    rate: u32,
    channels: u32,
    float: bool,
    bits: u32,
}

/// Active capture endpoints for the tray menu: (device id, friendly name).
pub fn list_capture_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return out;
        };
        let Ok(coll) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return out };
        let count = coll.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = coll.Item(i) else { continue };
            let Ok(id_pw) = device.GetId() else { continue };
            let id = String::from_utf16_lossy(id_pw.as_wide());
            windows::Win32::System::Com::CoTaskMemFree(Some(id_pw.as_ptr() as *const _));
            let name = (|| -> Result<String> {
                use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
                use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
                let store = device.OpenPropertyStore(windows::Win32::System::Com::STGM_READ)?;
                let mut pv = store.GetValue(&PKEY_Device_FriendlyName)?;
                let s = pv.Anonymous.Anonymous.Anonymous.pwszVal;
                let name = if s.is_null() { String::new() } else { String::from_utf16_lossy(s.as_wide()) };
                let _ = PropVariantClear(&mut pv);
                Ok(name)
            })()
            .unwrap_or_default();
            let name = if name.is_empty() { format!("Microphone {}", i + 1) } else { name };
            out.push((id, name));
        }
    }
    out
}

/// Pull-mode mic capture; samples land in a stereo s16 FIFO at the system rate.
struct Mic {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: MixFormat,
    resample_pos: f64,
    tmp: Vec<i16>,
}

impl Drop for Mic {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

unsafe fn open_mic(enumerator: &IMMDeviceEnumerator, sel: &str) -> Result<Mic> {
    let device = if sel == "default" {
        enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?
    } else {
        let w: Vec<u16> = sel.encode_utf16().chain(std::iter::once(0)).collect();
        enumerator.GetDevice(PCWSTR(w.as_ptr()))?
    };
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let fmt_ptr = client.GetMixFormat()?;
    let fmt = parse_mix_format(fmt_ptr);
    // No event callback: drained on the 20 ms loop cadence (pull mode).
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, Default::default(), 2_000_000, 0, fmt_ptr, None)?;
    windows::Win32::System::Com::CoTaskMemFree(Some(fmt_ptr as *const _));
    let capture: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    crate::log!("audio: mic up ({} Hz, {} ch, float={})", fmt.rate, fmt.channels, fmt.float);
    Ok(Mic { client, capture, fmt, resample_pos: 0.0, tmp: Vec::new() })
}

/// Linear resample stereo s16 `src_rate` → `dst_rate` (fine for voice).
fn resample_into(src: &[i16], src_rate: u32, dst_rate: u32, pos: &mut f64, out: &mut std::collections::VecDeque<i16>) {
    let frames = src.len() / 2;
    if frames == 0 {
        return;
    }
    let step = src_rate as f64 / dst_rate as f64;
    let mut p = pos.max(0.0);
    while (p as usize) + 1 < frames {
        let i = p as usize;
        let f = p - i as f64;
        let l = src[i * 2] as f64 * (1.0 - f) + src[(i + 1) * 2] as f64 * f;
        let r = src[i * 2 + 1] as f64 * (1.0 - f) + src[(i + 1) * 2 + 1] as f64 * f;
        out.push_back(l as i16);
        out.push_back(r as i16);
        p += step;
    }
    *pos = p - frames as f64;
}

/// Drain pending mic packets into the FIFO. Err = device gone (drop the mic).
unsafe fn drain_mic(mic: &mut Mic, sys_rate: u32, fifo: &mut std::collections::VecDeque<i16>) -> Result<()> {
    loop {
        if mic.capture.GetNextPacketSize()? == 0 {
            return Ok(());
        }
        let mut data = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        mic.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
        let mut tmp = std::mem::take(&mut mic.tmp);
        convert_pcm(data, frames as usize, &mic.fmt, silent, &mut tmp);
        mic.capture.ReleaseBuffer(frames)?;
        if mic.fmt.rate == sys_rate {
            fifo.extend(tmp.iter().copied());
        } else {
            resample_into(&tmp, mic.fmt.rate, sys_rate, &mut mic.resample_pos, fifo);
        }
        mic.tmp = tmp;
        // Jitter bound: cap at 1 s, drop oldest.
        let cap = sys_rate as usize * 2;
        while fifo.len() > cap {
            fifo.pop_front();
        }
    }
}

/// Saturating mix of FIFO samples onto a system-audio block.
fn mix_fifo(pcm: &mut [i16], fifo: &mut std::collections::VecDeque<i16>) {
    for s in pcm.iter_mut() {
        let Some(m) = fifo.pop_front() else { break };
        *s = (*s as i32 + m as i32).clamp(-32768, 32767) as i16;
    }
}

unsafe fn parse_mix_format(fmt: *const WAVEFORMATEX) -> MixFormat {
    let f = &*fmt;
    let mut float = false;
    if f.wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE {
        let ext = fmt as *const WAVEFORMATEXTENSIBLE;
        let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
        float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    } else if f.wFormatTag == 3 {
        // WAVE_FORMAT_IEEE_FLOAT
        float = true;
    }
    MixFormat {
        rate: f.nSamplesPerSec,
        channels: f.nChannels as u32,
        float,
        bits: f.wBitsPerSample as u32,
    }
}

struct AacEncoder {
    mft: IMFTransform,
    out_sample: IMFSample,
    out_buf: IMFMediaBuffer,
    pub user_data: Vec<u8>,
    rate: u32,
}

impl AacEncoder {
    unsafe fn new(rate: u32) -> Result<AacEncoder> {
        let in_t = MFCreateMediaType()?;
        in_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        in_t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        in_t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        in_t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        in_t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        in_t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        in_t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * 4)?;

        let out_t = MFCreateMediaType()?;
        out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out_t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out_t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
        out_t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        out_t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out_t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SEC)?;
        out_t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;

        let reg_in = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
        let reg_out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_ALL, Some(&reg_in), Some(&reg_out), &mut activates, &mut count)?;
        if count == 0 || activates.is_null() {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "no AAC encoder"));
        }
        let list = std::slice::from_raw_parts_mut(activates, count as usize);
        let mft: IMFTransform = list[0].take().unwrap().ActivateObject()?;
        for a in list.iter_mut() {
            drop(a.take());
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

        mft.SetOutputType(0, &out_t, 0)?;
        mft.SetInputType(0, &in_t, 0)?;

        // AudioSpecificConfig for the MP4 esds box.
        let negotiated = mft.GetOutputCurrentType(0)?;
        let mut blob_ptr = std::ptr::null_mut();
        let mut blob_len = 0u32;
        let user_data = if negotiated.GetAllocatedBlob(&MF_MT_USER_DATA, &mut blob_ptr, &mut blob_len).is_ok()
            && !blob_ptr.is_null()
        {
            let v = std::slice::from_raw_parts(blob_ptr, blob_len as usize).to_vec();
            windows::Win32::System::Com::CoTaskMemFree(Some(blob_ptr as *const _));
            v
        } else {
            Vec::new()
        };

        let info = mft.GetOutputStreamInfo(0)?;
        let cb = info.cbSize.max(8192);
        let out_buf = MFCreateMemoryBuffer(cb)?;
        let out_sample = MFCreateSample()?;
        out_sample.AddBuffer(&out_buf)?;

        Ok(AacEncoder { mft, out_sample, out_buf, user_data, rate })
    }

    /// Feed interleaved s16 stereo; returns AAC packets for the ring.
    unsafe fn encode(&mut self, pcm: &[i16], pts: i64, out: &mut Vec<Packet>) -> Result<()> {
        let bytes = pcm.len() * 2;
        let buf = MFCreateMemoryBuffer(bytes as u32)?;
        let mut ptr = std::ptr::null_mut();
        buf.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, bytes);
        buf.Unlock()?;
        buf.SetCurrentLength(bytes as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buf)?;
        sample.SetSampleTime(pts)?;
        sample.SetSampleDuration((pcm.len() as i64 / 2) * SEC / self.rate as i64)?;
        self.mft.ProcessInput(0, &sample, 0)?;
        self.drain(out)
    }

    unsafe fn drain(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        loop {
            self.out_buf.SetCurrentLength(0)?;
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(self.out_sample.clone())),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut bufs, &mut status);
            // Balance the clone we passed in.
            std::mem::ManuallyDrop::drop(&mut bufs[0].pSample);
            std::mem::ManuallyDrop::drop(&mut bufs[0].pEvents);
            match r {
                Ok(()) => {
                    let pts = self.out_sample.GetSampleTime().unwrap_or(0);
                    let dur = self
                        .out_sample
                        .GetSampleDuration()
                        .unwrap_or(1024 * SEC / self.rate as i64);
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    self.out_buf.Lock(&mut ptr, None, Some(&mut len))?;
                    let data: Arc<[u8]> = std::slice::from_raw_parts(ptr, len as usize).into();
                    let _ = self.out_buf.Unlock();
                    if !data.is_empty() {
                        out.push(Packet { kind: StreamKind::Audio, pts, dur, keyframe: true, data });
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

use crate::frames::qpc_now;

fn run_audio(
    ring: &Arc<Mutex<Ring>>,
    stop: &AtomicBool,
    meta: &Arc<Mutex<Option<AudioParams>>>,
    mic_sel: &str,
) -> Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt_ptr = client.GetMixFormat()?;
        let mix = parse_mix_format(fmt_ptr);
        crate::log!("audio: mix format {} Hz, {} ch, float={}, {} bits", mix.rate, mix.channels, mix.float, mix.bits);
        if mix.rate != 48_000 && mix.rate != 44_100 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "unsupported mix sample rate for AAC",
            ));
        }
        if !mix.float && mix.bits != 16 && mix.bits != 32 {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "unsupported mix format"));
        }

        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            2_000_000, // 200 ms buffer
            0,
            fmt_ptr,
            None,
        )?;
        windows::Win32::System::Com::CoTaskMemFree(Some(fmt_ptr as *const _));
        let event: HANDLE = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        let mut aac = AacEncoder::new(mix.rate)?;
        *meta.lock().unwrap() = Some(AudioParams {
            sample_rate: mix.rate,
            channels: 2,
            bytes_per_sec: AAC_BYTES_PER_SEC,
            user_data: aac.user_data.clone(),
        });
        crate::log!("audio leg: up ({} Hz AAC {} kbps)", mix.rate, AAC_BYTES_PER_SEC * 8 / 1000);

        // Mic is best-effort: failure to open or mid-run death never kills the leg.
        let mut mic: Option<Mic> = if mic_sel != "off" {
            match open_mic(&enumerator, mic_sel) {
                Ok(m) => Some(m),
                Err(e) => {
                    crate::log!("audio: mic open failed ({e}), recording system audio only");
                    None
                }
            }
        } else {
            None
        };
        let mut mic_fifo = std::collections::VecDeque::<i16>::new();

        let mut next_pts: i64 = 0; // timeline cursor for fed PCM
        let mut pcm = Vec::<i16>::with_capacity(8192);
        let mut packets = Vec::<Packet>::new();
        let frames_to_100ns = |n: i64| n * SEC / mix.rate as i64;

        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            let wait = WaitForSingleObject(event, 20);
            if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
                break Err(windows::core::Error::from_thread());
            }

            if let Some(m) = mic.as_mut() {
                if let Err(e) = drain_mic(m, mix.rate, &mut mic_fifo) {
                    crate::log!("audio: mic lost ({e}), continuing without it");
                    mic = None;
                    mic_fifo.clear();
                }
            }

            // Drain all available real packets.
            loop {
                let n = capture.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let qpc = qpc as i64;
                if next_pts == 0 {
                    next_pts = qpc;
                }
                // Fill loopback gaps (silence while no app renders).
                let gap = qpc - next_pts;
                if gap > frames_to_100ns(256) {
                    let fill = (gap * mix.rate as i64 / SEC) as usize;
                    pcm.clear();
                    pcm.resize(fill.min(mix.rate as usize * 5) * 2, 0);
                    mix_fifo(&mut pcm, &mut mic_fifo);
                    aac.encode(&pcm, next_pts, &mut packets)?;
                    next_pts += frames_to_100ns(pcm.len() as i64 / 2);
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                convert_pcm(data, frames as usize, &mix, silent, &mut pcm);
                capture.ReleaseBuffer(frames)?;
                mix_fifo(&mut pcm, &mut mic_fifo);
                aac.encode(&pcm, next_pts, &mut packets)?;
                next_pts += frames_to_100ns(frames as i64);
            }

            // No packets for a while: synthesize silence up to now-margin.
            if next_pts > 0 {
                let now = qpc_now();
                let behind = now - SILENCE_MARGIN - next_pts;
                if behind > frames_to_100ns(1024) {
                    let fill = ((behind * mix.rate as i64 / SEC) as usize).min(mix.rate as usize / 2);
                    pcm.clear();
                    pcm.resize(fill * 2, 0);
                    mix_fifo(&mut pcm, &mut mic_fifo);
                    aac.encode(&pcm, next_pts, &mut packets)?;
                    next_pts += frames_to_100ns(fill as i64);
                }
            }

            if !packets.is_empty() {
                if let Ok(mut r) = ring.lock() {
                    for p in packets.drain(..) {
                        r.push(p);
                    }
                } else {
                    packets.clear();
                }
            }
        };

        let _ = client.Stop();
        let _ = windows::Win32::Foundation::CloseHandle(event);
        result
    }
}

/// Interleaved device format → interleaved s16 stereo.
fn convert_pcm(data: *const u8, frames: usize, mix: &MixFormat, silent: bool, out: &mut Vec<i16>) {
    out.clear();
    out.reserve(frames * 2);
    if silent || data.is_null() {
        out.resize(frames * 2, 0);
        return;
    }
    let ch = mix.channels as usize;
    unsafe {
        if mix.float && mix.bits == 32 {
            let s = std::slice::from_raw_parts(data as *const f32, frames * ch);
            for f in 0..frames {
                let l = s[f * ch];
                let r = if ch > 1 { s[f * ch + 1] } else { l };
                out.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                out.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
        } else if mix.bits == 16 {
            let s = std::slice::from_raw_parts(data as *const i16, frames * ch);
            for f in 0..frames {
                let l = s[f * ch];
                let r = if ch > 1 { s[f * ch + 1] } else { l };
                out.push(l);
                out.push(r);
            }
        } else {
            // 32-bit int PCM
            let s = std::slice::from_raw_parts(data as *const i32, frames * ch);
            for f in 0..frames {
                let l = (s[f * ch] >> 16) as i16;
                let r = if ch > 1 { (s[f * ch + 1] >> 16) as i16 } else { l };
                out.push(l);
                out.push(r);
            }
        }
    }
}
