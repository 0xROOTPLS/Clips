//! Encoded-sample ring buffer. Pure logic, no COM — fully unit-testable.
//! Holds compressed video+audio packets for the last `window` of wall time.

use std::collections::VecDeque;
use std::sync::Arc;

pub const SEC: i64 = 10_000_000; // 100 ns units
const GOP_SLACK: i64 = 3 * SEC; // keep a full GOP before the clip window edge

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamKind {
    Video,
    Audio,
}

#[derive(Clone)]
pub struct Packet {
    pub kind: StreamKind,
    pub pts: i64, // QPC-relative 100 ns, shared clock across streams
    pub dur: i64,
    pub keyframe: bool,
    pub data: Arc<[u8]>,
}

pub struct Ring {
    q: VecDeque<Packet>,
    bytes: usize,
    window: i64,
    byte_cap: usize,
    newest_video_pts: i64,
}

impl Ring {
    pub fn new(clip_seconds: u32, byte_cap: usize) -> Ring {
        Ring {
            q: VecDeque::new(),
            bytes: 0,
            window: clip_seconds as i64 * SEC + GOP_SLACK,
            byte_cap,
            newest_video_pts: 0,
        }
    }

    pub fn set_clip_seconds(&mut self, secs: u32) {
        self.window = secs as i64 * SEC + GOP_SLACK;
    }

    pub fn push(&mut self, p: Packet) {
        if p.kind == StreamKind::Video {
            self.newest_video_pts = p.pts;
        }
        self.bytes += p.data.len();
        self.q.push_back(p);
        let cutoff = self.newest_video_pts - self.window;
        while let Some(front) = self.q.front() {
            if (self.newest_video_pts > 0 && front.pts < cutoff) || self.bytes > self.byte_cap {
                self.bytes -= front.data.len();
                self.q.pop_front();
            } else {
                break;
            }
        }
    }

    /// Drop everything (encoder restart invalidates parameter sets).
    pub fn clear(&mut self) {
        self.q.clear();
        self.bytes = 0;
        self.newest_video_pts = 0;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// O(n) Arc clones under the lock; payloads are not copied.
    pub fn snapshot(&self) -> Vec<Packet> {
        self.q.iter().cloned().collect()
    }
}

pub struct Clip {
    pub video: Vec<Packet>,
    pub audio: Vec<Packet>,
    pub duration: i64,
}

/// Trim a snapshot to the last `clip_seconds`, starting on a video keyframe,
/// and rebase all PTS to zero. Returns None if there is no usable video.
pub fn trim(snapshot: &[Packet], clip_seconds: u32) -> Option<Clip> {
    let newest = snapshot
        .iter()
        .rev()
        .find(|p| p.kind == StreamKind::Video)
        .map(|p| p.pts)?;
    let cut = newest - clip_seconds as i64 * SEC;
    let start = snapshot
        .iter()
        .filter(|p| p.kind == StreamKind::Video && p.keyframe && p.pts <= cut)
        .map(|p| p.pts)
        .next_back()
        .or_else(|| {
            snapshot
                .iter()
                .find(|p| p.kind == StreamKind::Video && p.keyframe)
                .map(|p| p.pts)
        })?;

    let mut video = Vec::new();
    let mut audio = Vec::new();
    for p in snapshot {
        if p.pts < start {
            continue;
        }
        let mut p = p.clone();
        p.pts -= start;
        match p.kind {
            StreamKind::Video => video.push(p),
            StreamKind::Audio => audio.push(p),
        }
    }
    let duration = video.last().map(|p| p.pts + p.dur)?;
    Some(Clip { video, audio, duration })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(kind: StreamKind, pts: i64, keyframe: bool) -> Packet {
        Packet {
            kind,
            pts,
            dur: SEC / 60,
            keyframe,
            data: vec![0u8; 100].into(),
        }
    }

    /// 60 fps video with keyframe every 120 frames, 50 Hz audio.
    fn synthetic(seconds: i64) -> Vec<Packet> {
        let mut v = Vec::new();
        for i in 0..seconds * 60 {
            v.push(pkt(StreamKind::Video, i * SEC / 60, i % 120 == 0));
        }
        for i in 0..seconds * 50 {
            v.push(pkt(StreamKind::Audio, i * SEC / 50, true));
        }
        v.sort_by_key(|p| p.pts);
        v
    }

    #[test]
    fn evicts_by_window() {
        let mut r = Ring::new(15, usize::MAX);
        for p in synthetic(60) {
            r.push(p);
        }
        let snap = r.snapshot();
        let newest = snap.iter().rev().find(|p| p.kind == StreamKind::Video).unwrap().pts;
        let oldest = snap.first().unwrap().pts;
        assert!(newest - oldest <= 18 * SEC + SEC, "window too large: {}", newest - oldest);
        assert!(newest - oldest >= 15 * SEC, "window too small: {}", newest - oldest);
    }

    #[test]
    fn evicts_by_byte_cap() {
        let mut r = Ring::new(60, 5_000);
        for p in synthetic(10) {
            r.push(p);
        }
        assert!(r.bytes() <= 5_000);
    }

    #[test]
    fn trim_starts_on_keyframe_and_rebases() {
        let mut r = Ring::new(30, usize::MAX);
        for p in synthetic(40) {
            r.push(p);
        }
        let clip = trim(&r.snapshot(), 30).unwrap();
        let first = &clip.video[0];
        assert!(first.keyframe);
        assert_eq!(first.pts, 0);
        // Duration within [30, 32] s: keyframe granularity is 2 s.
        assert!(clip.duration >= 30 * SEC && clip.duration <= 32 * SEC + SEC, "{}", clip.duration);
        // Audio never precedes video start.
        assert!(clip.audio.iter().all(|a| a.pts >= 0));
        // PTS strictly monotonic per stream.
        assert!(clip.video.windows(2).all(|w| w[0].pts < w[1].pts));
    }

    #[test]
    fn trim_buffer_younger_than_window() {
        let snap = synthetic(5);
        let clip = trim(&snap, 30).unwrap();
        assert!(clip.video[0].keyframe);
        assert_eq!(clip.video[0].pts, 0);
        assert!(clip.duration <= 5 * SEC + SEC);
    }

    #[test]
    fn trim_no_keyframe_returns_none() {
        let snap: Vec<Packet> = (0..100)
            .map(|i| pkt(StreamKind::Video, i * SEC / 60, false))
            .collect();
        assert!(trim(&snap, 30).is_none());
    }

    #[test]
    fn trim_empty_returns_none() {
        assert!(trim(&[], 30).is_none());
        let audio_only: Vec<Packet> = (0..10).map(|i| pkt(StreamKind::Audio, i, true)).collect();
        assert!(trim(&audio_only, 30).is_none());
    }

    #[test]
    fn clear_resets() {
        let mut r = Ring::new(30, usize::MAX);
        for p in synthetic(5) {
            r.push(p);
        }
        r.clear();
        assert_eq!(r.bytes(), 0);
        assert!(r.snapshot().is_empty());
    }
}
