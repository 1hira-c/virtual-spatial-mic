//! Bounded audio transport and clock discipline, independent of Windows codecs.
use anyhow::{Result, ensure};
use std::{collections::VecDeque, sync::{Condvar, Mutex}, time::Duration};

pub(crate) const SAMPLE_RATE: u64 = 48_000;
const QUEUE_FRAME_LIMIT: u64 = SAMPLE_RATE * 5;
const QUEUE_PACKET_LIMIT: usize = 8_192;

#[derive(Debug)]
pub(crate) struct AudioPacket { pub samples: Vec<f32>, pub at: i64 }
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct QueueStats { pub frames: u64, pub peak: u64, pub rejected: u64 }
#[derive(Default)]
struct QueueState {
    packets: VecDeque<AudioPacket>, stats: QueueStats, stop: Option<i64>, closed: bool,
}
#[derive(Default)]
pub(crate) struct AudioQueue { state: Mutex<QueueState>, wake: Condvar }
impl AudioQueue {
    pub fn push(&self, samples: Vec<f32>, at: i64) -> bool {
        let frames = samples.len() as u64 / 2;
        let mut state = self.state.lock().unwrap();
        if state.closed { return true; }
        if state.stats.frames + frames > QUEUE_FRAME_LIMIT || state.packets.len() >= QUEUE_PACKET_LIMIT {
            state.stats.rejected += frames;
            return false;
        }
        state.stats.frames += frames;
        state.stats.peak = state.stats.peak.max(state.stats.frames);
        state.packets.push_back(AudioPacket { samples, at });
        self.wake.notify_one();
        true
    }
    // Terminal control never sits behind media or a full bounded queue.
    pub fn stop_at(&self, at: i64) {
        let mut state = self.state.lock().unwrap();
        state.stop = Some(state.stop.map_or(at, |previous| previous.min(at)));
        self.wake.notify_one();
    }
    pub fn stop(&self) -> Option<i64> { self.state.lock().unwrap().stop }
    pub fn close(&self) { self.state.lock().unwrap().closed = true; self.wake.notify_all(); }
    pub fn stats(&self) -> QueueStats { self.state.lock().unwrap().stats }
    pub fn pop(&self) -> Option<AudioPacket> {
        let mut state = self.state.lock().unwrap();
        let packet = state.packets.pop_front()?;
        state.stats.frames -= packet.samples.len() as u64 / 2;
        Some(packet)
    }
    pub fn wait(&self, wait: Duration) {
        let state = self.state.lock().unwrap();
        if state.packets.is_empty() && !state.closed {
            drop(self.wake.wait_timeout(state, wait).unwrap());
        }
    }
}

pub(crate) fn ns_to_frames(ns: i64) -> i64 {
    let value = ns as i128 * SAMPLE_RATE as i128;
    ((value + if value >= 0 { 500_000_000 } else { -500_000_000 }) / 1_000_000_000) as i64
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AudioSpan { pub start: i64, pub end: i64 }
/// QPC locates the stream; a sample clock carries it between packets. Timestamp
/// jitter must never be turned into repeated inserted zeros/deleted samples.
/// A slow phase filter corrects hardware-clock drift by tiny resampling changes.
#[derive(Default)]
pub(crate) struct AudioClock {
    input_frames: u64, phase: Option<f64>, previous: Option<(i64, usize)>, end: i64,
    observations: VecDeque<f64>,
    pub max_jitter_frames: f64, pub adjusted_frames: i64,
}
impl AudioClock {
    pub fn advance(&mut self, at: i64, frames: usize) -> Result<AudioSpan> {
        ensure!(frames > 0, "Empty video audio packet");
        if let Some((prior, count)) = self.previous {
            let jitter = (at - prior) as f64 * SAMPLE_RATE as f64 / 1e9 - count as f64;
            self.max_jitter_frames = self.max_jitter_frames.max(jitter.abs());
        }
        self.previous = Some((at, frames));
        let observed = at as f64 * SAMPLE_RATE as f64 / 1e9 - self.input_frames as f64;
        self.observations.push_back(observed);
        if self.observations.len() > 17 { self.observations.pop_front(); }
        let mut observations: Vec<_> = self.observations.iter().copied().collect();
        observations.sort_by(f64::total_cmp);
        let observed = observations[(observations.len() - 1) / 2];
        let (start, phase) = if let Some(previous) = self.phase {
            ensure!((observed - previous).abs() <= SAMPLE_RATE as f64 * 10., "Video audio clock jumped more than 10 seconds");
            let alpha = 1. - (-(frames as f64) / (SAMPLE_RATE as f64 * 10.)).exp();
            // Bound correction to 0.5% even when timestamps recover from a jump.
            let adjustment = ((observed - previous) * alpha).clamp(-(frames as f64) * 0.005, frames as f64 * 0.005);
            (self.end, previous + adjustment)
        } else { (ns_to_frames(at), observed) };
        self.phase = Some(phase);
        self.input_frames += frames as u64;
        let end = (self.input_frames as f64 + phase).round() as i64;
        ensure!(end >= start, "Video audio clock moved backwards");
        self.adjusted_frames += end - start - frames as i64;
        self.end = end;
        Ok(AudioSpan { start, end })
    }
}

/// Encode only current time when video work exceeds its frame budget. Replaying
/// every overdue frame would permanently starve audio and accumulate latency.
#[derive(Default)]
pub(crate) struct VideoPacer { pub next: u64, pub skipped: u64 }
impl VideoPacer {
    pub fn due(&mut self, origin: i64, now: i64, cutoff: Option<i64>, fps: u32) -> Option<u64> {
        let limit = cutoff.unwrap_or(now).min(now);
        let elapsed = (limit - origin).max(0) as u128;
        let slots = ((elapsed * fps as u128).div_ceil(1_000_000_000)) as u64;
        if self.next >= slots { return None; }
        let current = (((now - origin).max(0) as u128 * fps as u128) / 1_000_000_000) as u64;
        let slot = current.min(slots - 1).max(self.next);
        self.skipped += slot - self.next;
        self.next = slot + 1;
        Some(slot)
    }
    pub fn next_ns(&self, origin: i64, fps: u32) -> i64 {
        origin + (self.next as i128 * 1_000_000_000 / fps as i128) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamp_jitter_is_continuous_and_hour_long_drift_is_corrected() {
        let mut clock = AudioClock::default();
        let frames = 256;
        let total = 48_000 * 3_600 / frames;
        let mut previous = 0;
        for index in 0..total {
            // ±1ms jitter, plus a 100ppm independent audio hardware clock.
            let jitter = if index == 0 { 0 } else { ((index * 37) % 97) as i64 * 20_000 - 960_000 };
            let at = index as i64 * frames as i64 * 1_000_000_000 / 48_000;
            let span = clock.advance(at + at / 10_000 + jitter, frames).unwrap();
            assert_eq!(span.start, previous);
            assert!((255..=257).contains(&(span.end - span.start)));
            previous = span.end;
        }
        let expected = (total * frames) as i64 + (total * frames / 10_000) as i64;
        assert!((previous - expected).abs() < 100, "{previous} vs {expected}");
        assert!(clock.max_jitter_frames < 100.);
    }
    #[test]
    fn one_timestamp_outlier_does_not_insert_or_delete_audio() {
        let mut clock = AudioClock::default();
        clock.advance(0, 256).unwrap();
        let span = clock.advance(1_000_000_000 + 256 * 1_000_000_000 / 48_000, 256).unwrap();
        assert_eq!((span.start, span.end), (256, 512));
        let next = clock.advance(512 * 1_000_000_000 / 48_000, 256).unwrap();
        assert_eq!((next.start, next.end), (512, 768));
    }
    #[test]
    fn stop_bypasses_a_full_queue_and_overflow_is_observable() {
        let queue = AudioQueue::default();
        for index in 0..500 { assert!(queue.push(vec![0.; 960], index)); }
        assert!(!queue.push(vec![0.; 960], 501));
        queue.stop_at(77);
        assert_eq!(queue.stop(), Some(77));
        assert_eq!(queue.stats().peak, 240_000);
        assert_eq!(queue.stats().rejected, 480);
        while queue.pop().is_some() {}
        assert_eq!(queue.stats().frames, 0);
    }
    #[test]
    fn hour_of_slow_video_has_bounded_audio_and_no_historic_catchup() {
        let queue = AudioQueue::default();
        let mut pacer = VideoPacer::default();
        let end = 3_600_000_000_000i64;
        let mut next_audio = 0;
        let mut accepted = 0;
        let mut delivered = 0;
        let mut frames = 0;
        let mut now = 1;
        while now < end {
            while next_audio < now {
                assert!(queue.push(vec![0.; 512], next_audio));
                accepted += 256;
                next_audio = accepted * 1_000_000_000 / 48_000;
            }
            while let Some(packet) = queue.pop() { delivered += packet.samples.len() as i64 / 2; }
            if pacer.due(0, now, None, 30).is_some() { frames += 1; }
            // 4K readback/encoding takes 85ms: lower video fps, never lose voice.
            now += 85_000_000;
        }
        assert_eq!(accepted, delivered);
        assert!(queue.stats().peak <= 4_096);
        assert!(pacer.skipped > frames);
        assert!(frames < 50_000);
        assert_eq!(queue.stats().rejected, 0);
    }
}
