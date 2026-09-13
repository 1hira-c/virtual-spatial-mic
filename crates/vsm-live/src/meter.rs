//! Sample peaks accumulated between UI reads. Audio threads never wait for the UI.
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

#[derive(Default)]
struct Peak {
    value: AtomicU32,
    updated: AtomicI64,
}
impl Peak {
    fn push(&self, value: f32, now: i64) {
        // Nonnegative finite floats have the same ordering as their bit patterns.
        self.value.fetch_max(value.to_bits(), Ordering::Relaxed);
        self.updated.store(now, Ordering::Release);
    }
    fn take(&self, now: i64) -> f32 {
        let updated = self.updated.load(Ordering::Acquire);
        let peak = f32::from_bits(self.value.swap(0, Ordering::Relaxed));
        if updated == 0 || now.saturating_sub(updated) > 250_000_000 {
            0.
        } else {
            peak
        }
    }
}

#[derive(Default)]
pub struct Levels {
    input: Peak,
    left: Peak,
    right: Peak,
}
fn sample_peak(samples: impl Iterator<Item = f32>) -> f32 {
    samples
        .filter(|s| s.is_finite())
        .map(f32::abs)
        .fold(0., f32::max)
}
impl Levels {
    /// Input is the selected capture channel, before gain or spatial processing.
    pub fn input(&self, samples: &[f32], channels: usize, selected: usize, now: i64) {
        let peak = if channels == 0 || selected >= channels {
            0.
        } else {
            sample_peak(samples.chunks_exact(channels).map(|frame| frame[selected]))
        };
        self.input.push(peak, now);
    }
    /// Interleaved output after gain/limiting, independent of monitor playback.
    pub fn output(&self, samples: &[f32], now: i64) {
        self.left
            .push(sample_peak(samples.chunks_exact(2).map(|f| f[0])), now);
        self.right
            .push(sample_peak(samples.chunks_exact(2).map(|f| f[1])), now);
    }
    /// One consumer; returns linear sample peaks, not RMS or true peak.
    pub fn take(&self, now: i64) -> [f32; 3] {
        [
            self.input.take(now),
            self.left.take(now),
            self.right.take(now),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_channel_and_stereo_peaks_do_not_modify_audio() {
        let levels = Levels::default();
        let pcm = [-1.2, 0.25, 0.1, -0.5];
        levels.input(&pcm, 2, 1, 1);
        levels.output(&pcm, 1);
        assert_eq!(levels.take(2), [0.5, 1.2, 0.5]);
        assert_eq!(pcm, [-1.2, 0.25, 0.1, -0.5]);
        levels.input(&[0.75, -1.], 1, 0, 3);
        assert_eq!(levels.take(4), [1., 0., 0.]);
    }

    #[test]
    fn transients_survive_quiet_packets_and_reset_on_read() {
        let levels = Levels::default();
        levels.input(&[-1.], 1, 0, 1);
        levels.input(&[0.; 480], 1, 0, 10_000_001);
        assert_eq!(levels.take(50_000_001), [1., 0., 0.]);
        assert_eq!(levels.take(100_000_001), [0.; 3]);
    }

    #[test]
    fn stale_and_invalid_input_is_silent() {
        let levels = Levels::default();
        assert_eq!(levels.take(1), [0.; 3]);
        levels.input(&[1.], 1, 0, 1);
        levels.output(&[1., 1.], 1);
        assert_eq!(levels.take(250_000_002), [0.; 3]);
        levels.input(&[f32::NAN, f32::INFINITY], 1, 0, 300_000_000);
        levels.output(&[f32::NAN, f32::NEG_INFINITY], 300_000_000);
        assert_eq!(levels.take(300_000_001), [0.; 3]);
        levels.input(&[1.], 1, 1, 300_000_002);
        levels.input(&[1.], 0, 0, 300_000_003);
        assert_eq!(levels.take(300_000_004), [0.; 3]);
    }
}
