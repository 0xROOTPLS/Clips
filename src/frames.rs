//! Bounded frame queue between the WGC callback and the encoder thread,
//! plus the 60 fps pacer that thins WGC's change-driven delivery rate.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

/// QPC now in 100 ns units (same clock as WGC/WASAPI timestamps).
pub fn qpc_now() -> i64 {
    let mut t = 0i64;
    let mut f = 0i64;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut t);
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut f);
    }
    if f > 0 {
        (t as i128 * 10_000_000 / f as i128) as i64
    } else {
        0
    }
}

pub struct QFrame {
    pub tex: ID3D11Texture2D,
    pub pts: i64, // 100 ns QPC-relative
}

// SAFETY: texture is only wrapped into an MF sample on the consumer side;
// the device is multithread-protected.
unsafe impl Send for QFrame {}

const DEPTH: usize = 4;

#[derive(Default)]
pub struct FrameQueue {
    q: Mutex<VecDeque<QFrame>>,
    cv: Condvar,
}

impl FrameQueue {
    pub fn push(&self, f: QFrame) {
        let mut q = self.q.lock().unwrap();
        if q.len() >= DEPTH {
            q.pop_front(); // encoder is behind: newest wins
        }
        q.push_back(f);
        drop(q);
        self.cv.notify_one();
    }

    pub fn pop_timeout(&self, d: Duration) -> Option<QFrame> {
        let q = self.q.lock().unwrap();
        let (mut q, _) = self.cv.wait_timeout_while(q, d, |q| q.is_empty()).ok()?;
        q.pop_front()
    }

    pub fn notify(&self) {
        self.cv.notify_all();
    }
}

/// Accepts frames at most every `interval` (100 ns units). Not thread-safe;
/// lives inside the single WGC callback.
pub struct Pacer {
    interval: i64,
    next_due: i64,
}

impl Pacer {
    pub fn new(fps: u32) -> Pacer {
        Pacer { interval: 10_000_000 / fps.max(1) as i64, next_due: 0 }
    }

    /// Returns the grid-snapped PTS for an accepted frame, None to drop it.
    /// Snapping keeps inter-frame deltas exactly 1/fps within a run; gaps
    /// (static screen) re-anchor the grid at the real arrival time.
    pub fn accept(&mut self, pts: i64) -> Option<i64> {
        if pts < self.next_due {
            return None;
        }
        if self.next_due == 0 || pts > self.next_due + self.interval {
            self.next_due = pts + self.interval;
            Some(pts)
        } else {
            let out = self.next_due;
            self.next_due += self.interval;
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pacer;

    #[test]
    fn paces_360_to_60() {
        let mut p = Pacer::new(60);
        let step = 10_000_000 / 360; // 360 Hz input
        let out: Vec<i64> = (1..=360).filter_map(|i| p.accept(i * step)).collect();
        assert!((59..=61).contains(&out.len()), "got {}", out.len());
        // Snapped: constant deltas of exactly 1/60 s within the run.
        let interval = 10_000_000 / 60;
        assert!(out.windows(2).all(|w| w[1] - w[0] == interval));
    }

    #[test]
    fn passes_through_below_target() {
        let mut p = Pacer::new(60);
        let step = 10_000_000 / 30; // 30 Hz input
        let accepted = (1..=30).filter_map(|i| p.accept(i * step)).count();
        assert_eq!(accepted, 30);
    }

    #[test]
    fn reanchors_after_gap() {
        let mut p = Pacer::new(60);
        assert_eq!(p.accept(10), Some(10));
        assert_eq!(p.accept(50_000_000), Some(50_000_000)); // 5 s gap re-anchors
        assert_eq!(p.accept(50_050_000), None); // 5 ms later: too soon
        assert!(p.accept(50_200_000).is_some()); // 20 ms later: due, snapped
    }

    #[test]
    fn snapped_pts_monotonic() {
        let mut p = Pacer::new(60);
        let mut last = -1i64;
        for i in 1..1000 {
            if let Some(t) = p.accept(i * 27_000 + (i % 7) * 900) {
                assert!(t > last);
                last = t;
            }
        }
    }
}
