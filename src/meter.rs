use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cumulative byte counter with a ~1s instantaneous bitrate (bits/sec).
///
/// `tick()` must be called on a short interval (e.g. 200ms) so `bps()` has
/// samples to differentiate. `bps()` does not mutate, so polling `/status`
/// cannot skew the rate.
#[derive(Debug)]
pub struct ByteCounter {
    total: AtomicU64,
    started: Mutex<Option<Instant>>,
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub bytes: u64,
    /// Instantaneous bits/sec over ~1 second.
    pub bps: f64,
    /// Session-average bits/sec since first byte.
    pub bps_avg: f64,
    pub window_ms: u64,
}

impl Default for ByteCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl ByteCounter {
    pub fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            started: Mutex::new(None),
            samples: Mutex::new(VecDeque::new()),
        }
    }

    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        *self.started.lock().expect("meter started") = None;
        self.samples.lock().expect("meter samples").clear();
    }

    pub fn add(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.touch_start();
        self.total.fetch_add(n, Ordering::Relaxed);
    }

    /// Absolute total (ffmpeg `total_size=`).
    pub fn set(&self, n: u64) {
        self.touch_start();
        self.total.store(n, Ordering::Relaxed);
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn tick(&self) {
        let now = Instant::now();
        let t = self.total.load(Ordering::Relaxed);
        let mut s = self.samples.lock().expect("meter samples");
        s.push_back((now, t));
        while s.len() > 1 {
            if now.duration_since(s.front().unwrap().0) > Duration::from_millis(2500) {
                s.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn snapshot(&self) -> Rate {
        let now = Instant::now();
        let total = self.total.load(Ordering::Relaxed);
        let samples = self.samples.lock().expect("meter samples");
        let (t0, b0, window_ms) = if samples.is_empty() {
            (now, total, 0)
        } else {
            let target = now.checked_sub(Duration::from_millis(1000)).unwrap_or(now);
            let (t0, b0) = samples
                .iter()
                .find(|(t, _)| *t >= target)
                .or(samples.front())
                .copied()
                .unwrap();
            let window_ms = now.duration_since(t0).as_millis() as u64;
            (t0, b0, window_ms)
        };
        let dt = now.duration_since(t0).as_secs_f64().max(0.05);
        let bps = if window_ms < 50 {
            0.0
        } else {
            (total.saturating_sub(b0) as f64 * 8.0) / dt
        };
        let bps_avg = match *self.started.lock().expect("meter started") {
            Some(t) => {
                let dt = now.duration_since(t).as_secs_f64().max(0.05);
                (total as f64 * 8.0) / dt
            }
            None => 0.0,
        };
        Rate {
            bytes: total,
            bps,
            bps_avg,
            window_ms,
        }
    }

    fn touch_start(&self) {
        let mut s = self.started.lock().expect("meter started");
        if s.is_none() {
            *s = Some(Instant::now());
        }
    }
}

pub fn format_bps(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{:.2} Mbps", bps / 1_000_000.0)
    } else if bps >= 1_000.0 {
        format!("{:.1} kbps", bps / 1_000.0)
    } else {
        format!("{:.0} bps", bps)
    }
}

pub fn format_bytes(n: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    let x = n as f64;
    if x >= MIB {
        format!("{:.2} MiB", x / MIB)
    } else if x >= KIB {
        format!("{:.1} KiB", x / KIB)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn add_and_set() {
        let c = ByteCounter::new();
        c.add(100);
        c.add(50);
        assert_eq!(c.total(), 150);
        c.set(1000);
        assert_eq!(c.total(), 1000);
        c.reset();
        assert_eq!(c.total(), 0);
    }

    #[test]
    fn bps_from_ticks() {
        let c = ByteCounter::new();
        c.tick();
        for _ in 0..5 {
            thread::sleep(Duration::from_millis(50));
            c.add(1_000); // 1000 bytes / 50ms ≈ 160 kbps
            c.tick();
        }
        let r = c.snapshot();
        assert!(r.bytes >= 5_000);
        assert!(r.bps > 20_000.0 && r.bps < 2_000_000.0, "bps {}", r.bps);
        assert!(r.bps_avg > 0.0);
    }

    #[test]
    fn formatters() {
        assert_eq!(format_bps(2_500_000.0), "2.50 Mbps");
        assert_eq!(format_bps(4800.0), "4.8 kbps");
        assert!(format_bytes(3 * 1024 * 1024).contains("MiB"));
    }
}
