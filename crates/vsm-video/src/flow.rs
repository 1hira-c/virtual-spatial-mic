//! Bounded audio transport and clock discipline, independent of Windows codecs.
use anyhow::{Result, ensure};
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
    time::Duration,
};

pub(crate) const SAMPLE_RATE: u64 = 48_000;
const QUEUE_FRAME_LIMIT: u64 = SAMPLE_RATE * 5;
const QUEUE_PACKET_LIMIT: usize = 8_192;

#[derive(Debug)]
pub(crate) struct AudioPacket {
    pub samples: Vec<f32>,
    pub at: i64,
}
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct QueueStats {
    pub frames: u64,
    pub peak: u64,
    pub rejected: u64,
}
#[derive(Default)]
struct QueueState {
    packets: VecDeque<AudioPacket>,
    stats: QueueStats,
    stop: Option<i64>,
    closed: bool,
}
#[derive(Default)]
pub(crate) struct AudioQueue {
    state: Mutex<QueueState>,
    wake: Condvar,
}
impl AudioQueue {
    pub fn push(&self, samples: Vec<f32>, at: i64) -> bool {
        let frames = samples.len() as u64 / 2;
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return true;
        }
        if state.stats.frames + frames > QUEUE_FRAME_LIMIT
            || state.packets.len() >= QUEUE_PACKET_LIMIT
        {
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
    pub fn stop(&self) -> Option<i64> {
        self.state.lock().unwrap().stop
    }
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.wake.notify_all();
    }
    pub fn stats(&self) -> QueueStats {
        self.state.lock().unwrap().stats
    }
    pub fn front_timing(&self) -> Option<(i64, usize)> {
        self.state
            .lock()
            .unwrap()
            .packets
            .front()
            .map(|packet| (packet.at, packet.samples.len() / 2))
    }
    // Positive alignment holds media already queued. Still wait for the clock or
    // notification; the ordinary nonempty-queue path would otherwise busy-spin.
    pub fn wait_until_due(&self, wait: Duration) {
        let state = self.state.lock().unwrap();
        if !state.closed {
            drop(self.wake.wait_timeout(state, wait).unwrap());
        }
    }
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
    ((value
        + if value >= 0 {
            500_000_000
        } else {
            -500_000_000
        })
        / 1_000_000_000) as i64
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AudioSpan {
    pub start: i64,
    pub end: i64,
}
/// QPC locates the stream; a sample clock carries it between packets. Timestamp
/// jitter must never be turned into repeated inserted zeros/deleted samples.
/// A slow phase filter corrects hardware-clock drift by tiny resampling changes.
#[derive(Clone, Default)]
pub(crate) struct AudioClock {
    input_frames: u64,
    phase: Option<f64>,
    previous: Option<(i64, usize)>,
    end: i64,
    observations: VecDeque<f64>,
    pub max_jitter_frames: f64,
    pub adjusted_frames: i64,
}
impl AudioClock {
    /// Preview without advancing the clock: a delayed packet must not be written
    /// beyond wall time while the user can still stop at that earlier time.
    pub fn due(&self, at: i64, frames: usize, now: i64) -> Result<bool> {
        let span = self.clone().advance(at, frames)?;
        Ok(span.end <= ns_to_frames(now))
    }
    pub fn advance(&mut self, at: i64, frames: usize) -> Result<AudioSpan> {
        ensure!(frames > 0, "Empty video audio packet");
        if let Some((prior, count)) = self.previous {
            let jitter = (at - prior) as f64 * SAMPLE_RATE as f64 / 1e9 - count as f64;
            self.max_jitter_frames = self.max_jitter_frames.max(jitter.abs());
        }
        self.previous = Some((at, frames));
        let observed = at as f64 * SAMPLE_RATE as f64 / 1e9 - self.input_frames as f64;
        self.observations.push_back(observed);
        if self.observations.len() > 17 {
            self.observations.pop_front();
        }
        let mut observations: Vec<_> = self.observations.iter().copied().collect();
        observations.sort_by(f64::total_cmp);
        let observed = observations[(observations.len() - 1) / 2];
        let (start, phase) = if let Some(previous) = self.phase {
            ensure!(
                (observed - previous).abs() <= SAMPLE_RATE as f64 * 10.,
                "Video audio clock jumped more than 10 seconds"
            );
            let alpha = 1. - (-(frames as f64) / (SAMPLE_RATE as f64 * 10.)).exp();
            // Bound correction to 0.5% even when timestamps recover from a jump.
            let adjustment = ((observed - previous) * alpha)
                .clamp(-(frames as f64) * 0.005, frames as f64 * 0.005);
            (self.end, previous + adjustment)
        } else {
            (ns_to_frames(at), observed)
        };
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
pub(crate) struct VideoPacer {
    pub next: u64,
    pub skipped: u64,
}
impl VideoPacer {
    pub fn due(&mut self, origin: i64, now: i64, cutoff: Option<i64>, fps: u32) -> Option<u64> {
        let limit = cutoff.unwrap_or(now).min(now);
        let elapsed = (limit - origin).max(0) as u128;
        let slots = ((elapsed * fps as u128).div_ceil(1_000_000_000)) as u64;
        if self.next >= slots {
            return None;
        }
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

/// Presentation times come from acquisition, not the work-gating frame slot.
/// Rounding loses less than 100ns; a held frame at origin is valid because the
/// image already existed then. An acquired future image is never backdated.
pub(crate) struct VideoTimeline {
    origin_qpc_ns: i64,
    pub last_pts_100ns: Option<i64>,
}
impl VideoTimeline {
    pub fn new(origin_qpc_ns: i64) -> Self {
        Self {
            origin_qpc_ns,
            last_pts_100ns: None,
        }
    }
    pub fn acquired(&mut self, acquired_qpc_ns: i64, cutoff_qpc_ns: Option<i64>) -> Option<i64> {
        if cutoff_qpc_ns.is_some_and(|end| acquired_qpc_ns >= end) {
            return None;
        }
        let time = (acquired_qpc_ns - self.origin_qpc_ns).max(0) / 100;
        if self.last_pts_100ns.is_some_and(|last| time <= last) {
            return None;
        }
        self.last_pts_100ns = Some(time);
        Some(time)
    }
}
pub(crate) fn video_duration_100ns(start: i64, next_or_cutoff: i64) -> Result<i64> {
    ensure!(
        next_or_cutoff > start,
        "Video sample timestamps are not increasing"
    );
    Ok(next_or_cutoff - start)
}

/// Map the unchanged source/capture clock into the MP4 timeline only.
pub(crate) fn audio_relative_time_ns(source: i64, origin: i64, offset: i64) -> Result<i64> {
    source
        .checked_sub(origin)
        .and_then(|at| at.checked_add(offset))
        .ok_or_else(|| anyhow::anyhow!("Video audio timestamp overflow"))
}

/// A negative placement offset requires real future source audio at the end.
/// The queue remains streaming/bounded: this is a deadline, not an audio buffer.
pub(crate) fn audio_drain_deadline_ns(now: i64, cutoff: i64, offset: i64) -> i64 {
    now.max(cutoff)
        .saturating_add(350_000_000)
        .saturating_add(offset.saturating_neg().max(0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AudioOutputBounds {
    pub start: u64,
    pub end: u64,
}
/// Clip shifted audio against MP4 start/stop before inserting any leading silence.
pub(crate) fn audio_output_bounds(
    span: AudioSpan,
    written: u64,
    cutoff_frames: Option<u64>,
) -> Option<AudioOutputBounds> {
    let start = span.start.max(0) as u64;
    let mut end = span.end.max(0) as u64;
    if let Some(cutoff) = cutoff_frames {
        end = end.min(cutoff);
    }
    if end <= written || end <= start {
        return None;
    }
    Some(AudioOutputBounds { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn positive_offset_waits_for_destination_time_without_advancing_clock() {
        let clock = AudioClock::default();
        assert!(!clock.due(2_000_000_000, 256, 1_000_000_000).unwrap());
        assert!(!clock.due(2_000_000_000, 256, 2_000_000_000).unwrap());
        assert!(clock.due(2_000_000_000, 256, 2_006_000_000).unwrap());
        assert_eq!(clock.input_frames, 0);
        assert!(clock.phase.is_none());
        let queue = AudioQueue::default();
        assert!(queue.push(vec![0.25; 512], 2_000_000_000));
        assert_eq!(queue.front_timing(), Some((2_000_000_000, 256)));
        assert_eq!(queue.stats().frames, 256);
        // A stop before a delayed packet becomes due can discard it, with no
        // future silence/samples already committed to the file.
        let mut active_clock = clock;
        let span = active_clock.advance(2_000_000_000, 256).unwrap();
        assert_eq!(audio_output_bounds(span, 0, Some(48_000)), None);
    }
    #[test]
    fn manual_audio_offset_sign_and_start_boundary() {
        let origin = 10_000_000_000;
        let mut delayed = AudioClock::default();
        let delayed_span = delayed
            .advance(
                audio_relative_time_ns(origin, origin, 200_000_000).unwrap(),
                256,
            )
            .unwrap();
        assert_eq!(
            audio_output_bounds(delayed_span, 0, Some(48_000)),
            Some(AudioOutputBounds {
                start: 9_600,
                end: 9_856
            })
        );
        let mut advanced = AudioClock::default();
        let discarded = advanced
            .advance(
                audio_relative_time_ns(origin, origin, -200_000_000).unwrap(),
                9_600,
            )
            .unwrap();
        assert_eq!(audio_output_bounds(discarded, 0, Some(48_000)), None);
        let boundary = advanced
            .advance(
                audio_relative_time_ns(origin + 200_000_000, origin, -200_000_000).unwrap(),
                256,
            )
            .unwrap();
        assert_eq!(
            audio_output_bounds(boundary, 0, Some(48_000)),
            Some(AudioOutputBounds { start: 0, end: 256 })
        );
        // Delay beyond the entire recording must not insert silence beyond stop.
        assert_eq!(
            audio_output_bounds(
                AudioSpan {
                    start: 96_000,
                    end: 96_256
                },
                0,
                Some(48_000)
            ),
            None
        );
    }
    #[test]
    fn negative_offset_drains_future_source_without_retaining_a_tail_buffer() {
        let origin = 10_000_000_000;
        let cutoff = origin + 1_000_000_000;
        let offset = -2_000_000_000;
        assert_eq!(
            audio_drain_deadline_ns(cutoff, cutoff, offset),
            origin + 3_350_000_000
        );
        assert_eq!(
            audio_drain_deadline_ns(cutoff, cutoff, 2_000_000_000),
            origin + 1_350_000_000
        );
        let queue = AudioQueue::default();
        queue.stop_at(cutoff);
        let mut clock = AudioClock::default();
        let mut written = 0;
        let mut first = 0;
        while first < 3 * SAMPLE_RATE {
            let count = 256.min(3 * SAMPLE_RATE - first);
            let source = origin + (first as i128 * 1_000_000_000 / SAMPLE_RATE as i128) as i64;
            assert!(source + 100_000_000 < audio_drain_deadline_ns(cutoff, cutoff, offset));
            assert!(queue.push(vec![0.25; count as usize * 2], source));
            let packet = queue.pop().unwrap();
            let span = clock
                .advance(
                    audio_relative_time_ns(packet.at, origin, offset).unwrap(),
                    packet.samples.len() / 2,
                )
                .unwrap();
            if let Some(bounds) = audio_output_bounds(span, written, Some(SAMPLE_RATE)) {
                written = bounds.end;
            }
            first += count;
        }
        assert_eq!(written, SAMPLE_RATE);
        assert_eq!(queue.stats().rejected, 0);
        assert_eq!(queue.stats().peak, 256);
        assert_eq!(queue.stats().frames, 0);
    }
    #[test]
    fn manual_audio_offset_is_placement_only_and_timestamp_overflow_is_reported() {
        let source = 12_345_678_901;
        let origin = 12_000_000_000;
        assert_eq!(
            audio_relative_time_ns(source, origin, 200_000_000).unwrap(),
            545_678_901
        );
        assert_eq!(source, 12_345_678_901);
        assert!(audio_relative_time_ns(i64::MAX, -1, 0).is_err());
        assert!(audio_relative_time_ns(i64::MAX, 0, 1).is_err());
    }
    #[test]
    fn stalled_capture_never_assigns_future_image_to_pre_read_tick() {
        let origin = 1_000_000_000;
        let mut gate = VideoPacer::default();
        assert_eq!(gate.due(origin, origin + 1, None, 30), Some(0));
        let mut timeline = VideoTimeline::new(origin);
        let acquired_after_mutex_stall = origin + 80_000_123;
        let time = timeline.acquired(acquired_after_mutex_stall, None).unwrap();
        assert_eq!(time, 800_001);
        assert!(time * 100 <= acquired_after_mutex_stall - origin);
        assert!(acquired_after_mutex_stall - origin - time * 100 < 100);
        assert_ne!(time, 0);
    }
    #[test]
    fn duplicate_and_backwards_acquisition_do_not_overlap_video_samples() {
        let mut timeline = VideoTimeline::new(1_000);
        assert_eq!(timeline.acquired(999, None), Some(0));
        assert_eq!(timeline.acquired(1_000, None), None);
        assert_eq!(timeline.acquired(1_101, None), Some(1));
        assert_eq!(timeline.acquired(1_100, None), None);
        assert_eq!(timeline.acquired(1_001, None), None);
    }
    #[test]
    fn stop_during_capture_rejects_future_frame_and_finishes_held_image() {
        let origin = 1_000_000_000;
        let mut timeline = VideoTimeline::new(origin);
        assert_eq!(timeline.acquired(origin, None), Some(0));
        assert_eq!(
            timeline.acquired(origin + 80_000_000, Some(origin + 60_000_000)),
            None
        );
        assert_eq!(timeline.last_pts_100ns, Some(0));
        assert_eq!(video_duration_100ns(0, 600_000).unwrap(), 600_000);
    }
    #[test]
    fn vfr_intervals_cover_gaps_and_exact_one_hour_cutoff() {
        assert_eq!(video_duration_100ns(0, 800_001).unwrap(), 800_001);
        assert_eq!(
            video_duration_100ns(800_001, 3_600_000_000 * 10).unwrap(),
            35_999_199_999
        );
        assert!(video_duration_100ns(33, 33).is_err());
        assert!(video_duration_100ns(33, 32).is_err());
    }
    #[test]
    fn timestamp_jitter_is_continuous_and_hour_long_drift_is_corrected() {
        let mut clock = AudioClock::default();
        let frames = 256;
        let total = 48_000 * 3_600 / frames;
        let mut previous = 0;
        for index in 0..total {
            // ±1ms jitter, plus a 100ppm independent audio hardware clock.
            let jitter = if index == 0 {
                0
            } else {
                ((index * 37) % 97) as i64 * 20_000 - 960_000
            };
            let at = index as i64 * frames as i64 * 1_000_000_000 / 48_000;
            let span = clock.advance(at + at / 10_000 + jitter, frames).unwrap();
            assert_eq!(span.start, previous);
            assert!((255..=257).contains(&(span.end - span.start)));
            previous = span.end;
        }
        let expected = (total * frames) as i64 + (total * frames / 10_000) as i64;
        assert!(
            (previous - expected).abs() < 100,
            "{previous} vs {expected}"
        );
        assert!(clock.max_jitter_frames < 100.);
    }
    #[test]
    fn one_timestamp_outlier_does_not_insert_or_delete_audio() {
        let mut clock = AudioClock::default();
        clock.advance(0, 256).unwrap();
        let span = clock
            .advance(1_000_000_000 + 256 * 1_000_000_000 / 48_000, 256)
            .unwrap();
        assert_eq!((span.start, span.end), (256, 512));
        let next = clock.advance(512 * 1_000_000_000 / 48_000, 256).unwrap();
        assert_eq!((next.start, next.end), (512, 768));
    }
    #[test]
    fn stop_bypasses_a_full_queue_and_overflow_is_observable() {
        let queue = AudioQueue::default();
        for index in 0..500 {
            assert!(queue.push(vec![0.; 960], index));
        }
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
            while let Some(packet) = queue.pop() {
                delivered += packet.samples.len() as i64 / 2;
            }
            if pacer.due(0, now, None, 30).is_some() {
                frames += 1;
            }
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
