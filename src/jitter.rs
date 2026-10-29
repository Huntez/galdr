//! Picks the playback buffer from how unevenly packets actually arrive.
//!
//! Each packet carries the phone's capture time (`timestamp`, in samples). Arrival time minus
//! capture time is the network delay plus an offset between the clocks. A packet is as late as
//! its offset is above the fastest one of the last couple of seconds (short enough that clock
//! drift doesn't count as lateness); the buffer has to cover the worst lateness seen lately.
//! A late packet that finds the buffer empty is an audible gap, so the target grows at once; it
//! shrinks slowly, so one quiet stretch of Wi-Fi doesn't undo what a burst taught.

use std::collections::VecDeque;
use std::time::Instant;

use crate::opus::SAMPLE_RATE;

/// What a lateness is measured against: 2 s, longer than any Wi-Fi stall worth riding out.
const BASELINE_MS: u32 = 2000;
/// How far back lateness is remembered.
const WINDOW_MS: u32 = 30_000;
/// Added to the worst lateness on top of half a packet (the queue level is a sawtooth around its
/// target): room for the output's own pulls and timer slop.
const SLOP_MS: f64 = 8.0;
/// Shrinking speed, per second of audio.
const SHRINK_MS_PER_S: f64 = 2.0;

pub struct Jitter {
    start: Instant,
    first_timestamp: Option<u32>,
    /// Arrival minus capture time of the packets of the last `BASELINE_MS`, ms.
    offsets: VecDeque<f64>,
    baseline: usize,
    /// Lateness of the packets of the last `WINDOW_MS`, ms.
    lateness: VecDeque<f64>,
    window: usize,
    margin_ms: f64,
    min_ms: u32,
    max_ms: u32,
    target_ms: f64,
    last_shrink: Instant,
}

impl Jitter {
    /// For packets of `frame_ms`; starts at `initial_ms` and stays within `min_ms..=max_ms`.
    pub fn new(frame_ms: u32, initial_ms: u32, min_ms: u32, max_ms: u32) -> Self {
        let now = Instant::now();
        let frame_ms = frame_ms.max(1);
        let (baseline, window) = (
            (BASELINE_MS / frame_ms) as usize,
            (WINDOW_MS / frame_ms) as usize,
        );
        Self {
            start: now,
            first_timestamp: None,
            offsets: VecDeque::with_capacity(baseline),
            baseline,
            lateness: VecDeque::with_capacity(window),
            window,
            margin_ms: frame_ms as f64 / 2.0 + SLOP_MS,
            min_ms,
            max_ms,
            target_ms: initial_ms.clamp(min_ms, max_ms) as f64,
            last_shrink: now,
        }
    }

    pub fn target_ms(&self) -> u32 {
        self.target_ms.round() as u32
    }

    /// Records a packet that just arrived; returns the new target when it changed.
    pub fn packet(&mut self, timestamp: u32) -> Option<u32> {
        self.packet_at(timestamp, Instant::now())
    }

    fn packet_at(&mut self, timestamp: u32, now: Instant) -> Option<u32> {
        let first = *self.first_timestamp.get_or_insert(timestamp);
        let captured_ms = timestamp.wrapping_sub(first) as f64 * 1000.0 / SAMPLE_RATE as f64;
        let arrived_ms = (now - self.start).as_secs_f64() * 1000.0;
        let offset = arrived_ms - captured_ms;
        if self.offsets.len() == self.baseline {
            self.offsets.pop_front();
        }
        self.offsets.push_back(offset);
        let fastest = self.offsets.iter().copied().fold(f64::INFINITY, f64::min);
        if self.lateness.len() == self.window {
            self.lateness.pop_front();
        }
        self.lateness.push_back(offset - fastest);

        let worst = self.lateness.iter().copied().fold(0.0, f64::max);
        let need = (worst + self.margin_ms).clamp(self.min_ms as f64, self.max_ms as f64);

        let before = self.target_ms();
        if need > self.target_ms {
            self.target_ms = need;
        } else {
            let elapsed = (now - self.last_shrink).as_secs_f64();
            self.target_ms = (self.target_ms - elapsed * SHRINK_MS_PER_S).max(need);
        }
        self.last_shrink = now;
        let after = self.target_ms();
        (after != before).then_some(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const FRAME: u32 = SAMPLE_RATE / 50;

    /// Feeds `n` packets 20 ms apart, each delayed by `late(i)` ms; returns the final target.
    fn run(j: &mut Jitter, base: Instant, from: u32, n: u32, late: impl Fn(u32) -> u64) -> u32 {
        for i in from..from + n {
            let at = base + Duration::from_millis(20 * i as u64 + late(i));
            j.packet_at(i * FRAME, at);
        }
        j.target_ms()
    }

    #[test]
    fn steady_packets_shrink_to_the_floor() {
        let base = Instant::now();
        let mut j = Jitter::new(20, 60, 20, 300);
        j.start = base;
        j.last_shrink = base;
        assert_eq!(run(&mut j, base, 0, 1500, |_| 0), 20);
    }

    #[test]
    fn clock_drift_is_not_lateness() {
        let base = Instant::now();
        let mut j = Jitter::new(20, 60, 20, 300);
        j.start = base;
        j.last_shrink = base;
        // The phone's clock 0.1% slow (far more than real ones): 1 ms later every 50 packets.
        assert_eq!(run(&mut j, base, 0, 1500, |i| i as u64 / 50), 20);
    }

    #[test]
    fn a_burst_raises_the_target_at_once_and_it_comes_down_slowly() {
        let base = Instant::now();
        let mut j = Jitter::new(20, 20, 20, 300);
        j.start = base;
        j.last_shrink = base;
        run(&mut j, base, 0, 100, |_| 0);
        // One stall: five packets held back and released together 60 ms late.
        let target = run(&mut j, base, 100, 5, |i| 60 - (i - 100) as u64 * 20 + 20);
        assert_eq!(target, 98); // 80 ms late + margin
        // 10 s of calm: down by ~20 ms, but the stall is still in the window.
        assert_eq!(run(&mut j, base, 105, 500, |_| 0), 98);
    }
}
