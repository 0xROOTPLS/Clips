//! Pass-through MP4 mux of already-encoded packets via the MF Sink Writer.

use std::path::Path;

use windows::core::{Result, PCWSTR};
use windows::Win32::Media::MediaFoundation::*;

use crate::ring::{Clip, Packet};

#[derive(Clone)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u32,
    pub bytes_per_sec: u32,
    pub user_data: Vec<u8>, // AudioSpecificConfig etc. from the AAC encoder's output type
}

pub struct MuxParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub seq_header: Option<Vec<u8>>,
    pub audio: Option<AudioParams>,
}

/// HEVC Annex-B scan: collect VPS(32)/SPS(33)/PPS(34) NALs up to the first VCL NAL.
pub fn extract_parameter_sets(annexb: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = annexb.len();
    let mut nal_start: Option<usize> = None;
    let mut flush = |start: usize, end: usize, out: &mut Vec<u8>| -> bool {
        if end <= start {
            return false;
        }
        let nal = &annexb[start..end];
        let ty = (nal[0] >> 1) & 0x3f;
        match ty {
            32..=34 => {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
                false
            }
            t if t < 32 => true, // VCL: parameter sets are done
            _ => false,
        }
    };
    while i + 3 <= n {
        let (sc, len) = if i + 4 <= n && annexb[i..i + 4] == [0, 0, 0, 1] {
            (true, 4)
        } else if annexb[i..i + 3] == [0, 0, 1] {
            (true, 3)
        } else {
            (false, 1)
        };
        if sc {
            if let Some(s) = nal_start.take() {
                if flush(s, i, &mut out) {
                    return (!out.is_empty()).then_some(out);
                }
            }
            i += len;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        let _ = flush(s, n, &mut out);
    }
    (!out.is_empty()).then_some(out)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn write_packet(writer: &IMFSinkWriter, stream: u32, p: &Packet) -> Result<()> {
    let buf = MFCreateMemoryBuffer(p.data.len() as u32)?;
    let mut ptr = std::ptr::null_mut();
    buf.Lock(&mut ptr, None, None)?;
    std::ptr::copy_nonoverlapping(p.data.as_ptr(), ptr, p.data.len());
    buf.Unlock()?;
    buf.SetCurrentLength(p.data.len() as u32)?;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buf)?;
    sample.SetSampleTime(p.pts)?;
    sample.SetSampleDuration(p.dur)?;
    if p.keyframe {
        sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
    }
    writer.WriteSample(stream, &sample)
}

pub fn save_clip(path: &Path, clip: &Clip, prm: &MuxParams) -> Result<()> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.unwrap();
        attrs.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?; // hard pass-through
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

        let path_w = wide(&path.display().to_string());
        let writer = MFCreateSinkWriterFromURL(PCWSTR(path_w.as_ptr()), None, Some(&attrs))?;

        let vt = MFCreateMediaType()?;
        vt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        vt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
        vt.SetUINT64(&MF_MT_FRAME_SIZE, ((prm.width as u64) << 32) | prm.height as u64)?;
        vt.SetUINT64(&MF_MT_FRAME_RATE, ((prm.fps as u64) << 32) | 1)?;
        vt.SetUINT32(&MF_MT_AVG_BITRATE, prm.bitrate)?;
        vt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        vt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        vt.SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
        let seq = prm
            .seq_header
            .clone()
            .or_else(|| clip.video.first().and_then(|p| extract_parameter_sets(&p.data)));
        let seq = seq.ok_or_else(|| {
            windows::core::Error::new(windows::Win32::Foundation::E_FAIL, "no HEVC parameter sets")
        })?;
        vt.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq)?;

        let v_idx = writer.AddStream(&vt)?;
        writer.SetInputMediaType(v_idx, &vt, None)?;

        let a_idx = if let (Some(a), true) = (prm.audio.as_ref(), !clip.audio.is_empty()) {
            let at = MFCreateMediaType()?;
            at.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            at.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            at.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, a.sample_rate)?;
            at.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, a.channels)?;
            at.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            at.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, a.bytes_per_sec)?;
            at.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            at.SetBlob(&MF_MT_USER_DATA, &a.user_data)?;
            let idx = writer.AddStream(&at)?;
            writer.SetInputMediaType(idx, &at, None)?;
            Some(idx)
        } else {
            None
        };

        writer.BeginWriting()?;

        // Two-pointer PTS merge for proper interleaving.
        let (mut vi, mut ai) = (0usize, 0usize);
        while vi < clip.video.len() || ai < clip.audio.len() {
            let take_video = match (clip.video.get(vi), clip.audio.get(ai)) {
                (Some(v), Some(a)) => v.pts <= a.pts || a_idx.is_none(),
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_video {
                write_packet(&writer, v_idx, &clip.video[vi])?;
                vi += 1;
            } else {
                if let Some(aidx) = a_idx {
                    write_packet(&writer, aidx, &clip.audio[ai])?;
                }
                ai += 1;
            }
        }

        writer.Finalize()
    }
}

/// `Clip 2026-06-11 18-04-32.mp4`
pub fn clip_filename() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "Clip {:04}-{:02}-{:02} {:02}-{:02}-{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::extract_parameter_sets;

    fn nal(ty: u8, payload: &[u8]) -> Vec<u8> {
        // HEVC NAL header: type in bits 6..1 of first byte.
        let mut v = vec![ty << 1, 0x01];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn extracts_vps_sps_pps() {
        let mut s = Vec::new();
        for (sc, ty) in [(4, 32u8), (3, 33), (4, 34), (4, 19)] {
            // 19 = IDR_W_RADL (VCL)
            s.extend_from_slice(if sc == 4 { &[0, 0, 0, 1][..] } else { &[0, 0, 1][..] });
            s.extend_from_slice(&nal(ty, &[0xAA, 0xBB]));
        }
        let ps = extract_parameter_sets(&s).unwrap();
        // Three parameter sets, each re-emitted with 4-byte start codes.
        let count = ps.windows(4).filter(|w| *w == [0, 0, 0, 1]).count();
        assert_eq!(count, 3);
        assert!(!ps.windows(2).any(|w| (w[0] >> 1) & 0x3f == 19 && w[1] == 0x01));
    }

    #[test]
    fn none_when_no_parameter_sets() {
        let mut s = vec![0, 0, 0, 1];
        s.extend_from_slice(&nal(19, &[1, 2, 3]));
        assert!(extract_parameter_sets(&s).is_none());
        assert!(extract_parameter_sets(&[]).is_none());
    }

    #[test]
    fn stops_at_first_vcl() {
        let mut s = Vec::new();
        s.extend_from_slice(&[0, 0, 0, 1]);
        s.extend_from_slice(&nal(33, &[1]));
        s.extend_from_slice(&[0, 0, 0, 1]);
        s.extend_from_slice(&nal(19, &[2]));
        s.extend_from_slice(&[0, 0, 0, 1]);
        s.extend_from_slice(&nal(34, &[3])); // PPS after VCL must be ignored
        let ps = extract_parameter_sets(&s).unwrap();
        let count = ps.windows(4).filter(|w| *w == [0, 0, 0, 1]).count();
        assert_eq!(count, 1);
    }
}
