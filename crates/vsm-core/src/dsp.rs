//! HRTF binaural rendering in pure Rust. The SADIE II HRIR set is embedded in
//! the binary and parsed once; one processor belongs to one audio worker.
use crate::math::{Vec3, dot, norm};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sofar::{
    reader::{Filter, OpenOptions, Sofar},
    render::Renderer,
};
use std::sync::OnceLock;

/// Original `D1_48K_24bit_256tap_FIR_SOFA.sofa`, zlib-compressed byte for byte.
static SOFA_ZLIB: &[u8] = include_bytes!("../assets/hrtf/SADIE-II-D1-48K-24bit-256tap.sofa.zlib");
pub const HRTF_SHA256: &str = "e6c72a84dd947b5ef75438ab96a9c2a32ed10f033472b9c4c11a49aff00a8a31";
pub const SPATIALIZER: &str = "sofar 0.3.0 / SADIE II D1 (KU100) 48kHz 256tap";
/// Uniformly partitioned convolution with partition == block adds no delay.
pub const LATENCY_FRAMES: usize = 0;
const RATE: f32 = 48000.;
/// Refilter when the direction moves by more than about 0.5 degrees.
const REFILTER_COS: f64 = 0.999_96;

struct Hrtf {
    sofa: Sofar,
    taps: usize,
    radius: f32,
}
static HRTF: OnceLock<std::result::Result<Hrtf, String>> = OnceLock::new();
fn load() -> Result<Hrtf> {
    let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(SOFA_ZLIB)
        .map_err(|e| anyhow!("HRTF data is damaged: {e:?}"))?;
    let sofa = OpenOptions::new()
        .sample_rate(RATE)
        .normalized(true)
        .open_data(&bytes)
        .context("HRTFデータを読み込めません")?;
    let taps = sofa.filter_len();
    ensure!(taps == 256, "Unexpected HRTF length {taps}");
    // sofar's renderer ignores per-measurement delays unless configured.
    ensure!(
        sofa.hrtf().data_delay.values.iter().all(|d| *d == 0.),
        "HRTF delays are unsupported"
    );
    let radius = sofa.lookup().radius_max;
    ensure!(radius.is_finite() && radius > 0., "Invalid HRTF radius");
    Ok(Hrtf { sofa, taps, radius })
}
fn hrtf() -> Result<&'static Hrtf> {
    HRTF.get_or_init(|| load().map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}
/// Parse the embedded HRTF ahead of the first processor.
pub fn preload() -> Result<()> {
    hrtf().map(drop)
}
/// Listener frame (+X right, +Y up, -Z forward) to SOFA (+X front, +Y left, +Z up).
fn to_sofa(u: Vec3) -> [f32; 3] {
    [-u[2] as f32, -u[0] as f32, u[1] as f32]
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Accepting,
    Draining,
    Complete,
}
struct Voice {
    renderer: Renderer,
    left: Vec<f32>,
    right: Vec<f32>,
}
pub struct Binaural {
    hrtf: &'static Hrtf,
    voices: [Voice; 2],
    active: usize,
    filter: Filter,
    direction: Option<Vec3>,
    block: usize,
    bounded_distance: bool,
    mono: Vec<f32>,
    state: State,
    previous_gain: f64,
    has_input: bool,
    tail: usize,
}
impl Binaural {
    pub fn new(block: usize, bounded_distance: bool) -> Result<Self> {
        ensure!(
            [128, 256, 512, 1024].contains(&block),
            "Supported block sizes: 128,256,512,1024"
        );
        let hrtf = hrtf()?;
        let voice = || -> Result<Voice> {
            Ok(Voice {
                renderer: Renderer::builder(hrtf.taps)
                    .with_sample_rate(RATE)
                    .with_partition_len(block)
                    .build()?,
                left: vec![0.; block],
                right: vec![0.; block],
            })
        };
        Ok(Self {
            hrtf,
            voices: [voice()?, voice()?],
            active: 0,
            filter: Filter::new(hrtf.taps),
            direction: None,
            block,
            bounded_distance,
            mono: vec![0.; block],
            state: State::Accepting,
            previous_gain: 1.,
            has_input: false,
            tail: 0,
        })
    }
    /// Loads the filter for `unit`; true means crossfade to the standby voice.
    fn steer(&mut self, unit: Vec3) -> Result<bool> {
        if self.direction.is_some_and(|d| dot(d, unit) >= REFILTER_COS) {
            return Ok(false);
        }
        let [x, y, z] = to_sofa(unit).map(|v| v * self.hrtf.radius);
        self.hrtf.sofa.filter(x, y, z, &mut self.filter);
        ensure!(
            self.filter
                .left
                .iter()
                .chain(&self.filter.right)
                .any(|v| *v != 0.),
            "HRTF lookup failed"
        );
        let fade = self.direction.is_some();
        let target = if fade { 1 - self.active } else { self.active };
        self.voices[target].renderer.set_filter(&self.filter)?;
        self.direction = Some(unit);
        Ok(fade)
    }
    fn render(&mut self, fade: bool) -> Result<()> {
        // Both voices always see the same input, and set_filter keeps that
        // history, so the standby voice is ready to take over at any block.
        for v in &mut self.voices {
            v.renderer
                .process_block(&self.mono[..], &mut v.left[..], &mut v.right[..])?;
        }
        if fade {
            let [a, b] = &mut self.voices;
            let (from, to) = if self.active == 0 { (a, b) } else { (b, a) };
            for n in 0..self.block {
                let w = (n + 1) as f32 / self.block as f32;
                to.left[n] = from.left[n] + (to.left[n] - from.left[n]) * w;
                to.right[n] = from.right[n] + (to.right[n] - from.right[n]) * w;
            }
            self.active = 1 - self.active;
        }
        Ok(())
    }
    fn interleave(&self, output: &mut [f32], frames: usize) {
        let v = &self.voices[self.active];
        for i in 0..frames {
            output[i * 2] = v.left[i];
            output[i * 2 + 1] = v.right[i];
        }
    }
    fn finite(&self) -> bool {
        let v = &self.voices[self.active];
        v.left.iter().chain(&v.right).all(|v| v.is_finite())
    }
    pub fn process(&mut self, input: &[f32], output: &mut [f32], position: Vec3) -> Result<()> {
        output.fill(0.);
        ensure!(
            input.len() == self.block && output.len() == self.block * 2,
            "Invalid DSP block size"
        );
        ensure!(self.state == State::Accepting, "Stream finished");
        let distance = norm(position);
        ensure!(
            position.iter().all(|v| v.is_finite()) && distance.is_finite() && distance >= 1e-6,
            "Invalid source position"
        );
        ensure!(input.iter().all(|v| v.is_finite()), "Nonfinite audio");
        let gain = if self.bounded_distance {
            4f64.min(1. / distance.max(0.25))
        } else {
            1.
        };
        let previous = if self.has_input {
            self.previous_gain
        } else {
            gain
        };
        for (i, v) in input.iter().enumerate() {
            self.mono[i] = (*v as f64
                * (previous + (gain - previous) * (i + 1) as f64 / self.block as f64))
                as f32;
            ensure!(self.mono[i].is_finite(), "Gain overflow");
        }
        let fade = self.steer(position.map(|v| v / distance))?;
        self.render(fade)?;
        if !self.finite() {
            self.reset();
            bail!("Nonfinite binaural output");
        }
        self.interleave(output, self.block);
        self.previous_gain = gain;
        self.has_input = true;
        Ok(())
    }
    pub fn drain(&mut self, output: &mut [f32]) -> Result<(usize, bool)> {
        output.fill(0.);
        ensure!(output.len() == self.block * 2, "Invalid DSP block size");
        if !self.has_input || self.state == State::Complete {
            self.state = State::Complete;
            return Ok((0, true));
        }
        if self.state == State::Accepting {
            self.state = State::Draining;
            self.tail = self.hrtf.taps - 1;
        }
        self.mono.fill(0.);
        self.render(false)?;
        if !self.finite() {
            self.reset();
            bail!("Nonfinite binaural tail");
        }
        let frames = self.block.min(self.tail);
        self.interleave(output, frames);
        self.tail -= frames;
        let complete = self.tail == 0;
        if complete {
            self.state = State::Complete;
        }
        Ok((frames, complete))
    }
    pub fn reset(&mut self) {
        for v in &mut self.voices {
            v.renderer.reset();
            v.left.fill(0.);
            v.right.fill(0.);
        }
        self.mono.fill(0.);
        self.active = 0;
        self.direction = None;
        self.state = State::Accepting;
        self.previous_gain = 1.;
        self.has_input = false;
        self.tail = 0;
    }
    pub fn latency_frames(&self) -> usize {
        LATENCY_FRAMES
    }
}

pub struct Level {
    envelope: f64,
    pub limited_frames: u64,
}
impl Default for Level {
    fn default() -> Self {
        Self {
            envelope: 1.,
            limited_frames: 0,
        }
    }
}
impl Level {
    pub fn apply(&mut self, stereo: &mut [f32], gain: f64) -> Result<()> {
        self.apply_ramp(stereo, gain, gain)
    }
    pub fn apply_ramp(&mut self, stereo: &mut [f32], from: f64, gain: f64) -> Result<()> {
        ensure!(
            stereo.len() % 2 == 0
                && gain.is_finite()
                && (0.0..=1.0).contains(&gain)
                && from.is_finite()
                && (0.0..=1.0).contains(&from),
            "Invalid output level"
        );
        let frames = stereo.len() / 2;
        for (i, pair) in stereo.chunks_exact_mut(2).enumerate() {
            let gain = from + (gain - from) * (i + 1) as f64 / frames as f64;
            let l = pair[0] as f64 * gain;
            let r = pair[1] as f64 * gain;
            ensure!(l.is_finite() && r.is_finite(), "Nonfinite output");
            let peak = l.abs().max(r.abs());
            let target = if peak > 0.98 { 0.98 / peak } else { 1. };
            self.envelope = target.min(self.envelope + (1. - self.envelope) / 2400.);
            if self.envelope < 1. {
                self.limited_frames += 1;
            }
            pair[0] = (l * self.envelope) as f32;
            pair[1] = (r * self.envelope) as f32;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const LEFT: Vec3 = [-1., 0., 0.];
    const RIGHT: Vec3 = [1., 0., 0.];
    const FRONT: Vec3 = [0., 0., -1.];

    fn noise(len: usize, seed: &mut u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (*seed >> 8) as f32 / (1 << 23) as f32 - 1.
            })
            .collect()
    }
    fn render(b: &mut Binaural, input: &[f32], position: Vec3) -> Result<Vec<f32>> {
        let mut out = Vec::new();
        let mut block = vec![0.; b.block * 2];
        for chunk in input.chunks(b.block) {
            b.process(chunk, &mut block, position)?;
            out.extend_from_slice(&block);
        }
        Ok(out)
    }
    fn rms(stereo: &[f32], channel: usize) -> f64 {
        let s: f64 = stereo
            .iter()
            .skip(channel)
            .step_by(2)
            .map(|v| (*v as f64).powi(2))
            .sum();
        (s / (stereo.len() / 2) as f64).sqrt()
    }

    #[test]
    fn embedded_hrtf_integrity() -> Result<()> {
        let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(SOFA_ZLIB).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), HRTF_SHA256);
        let h = hrtf()?;
        assert_eq!(h.taps, 256);
        assert!((h.radius - 1.2).abs() < 1e-3);
        assert_eq!(h.sofa.sample_rate(), RATE);
        Ok(())
    }
    #[test]
    fn listener_to_sofa_axes() {
        assert_eq!(to_sofa(FRONT), [1., 0., 0.]);
        assert_eq!(to_sofa(RIGHT), [0., -1., 0.]);
        assert_eq!(to_sofa([0., 1., 0.]), [0., 0., 1.]);
    }
    #[test]
    fn impulse_response_matches_filter_all_blocks() -> Result<()> {
        let h = hrtf()?;
        let mut expected = Filter::new(h.taps);
        let [x, y, z] = to_sofa(RIGHT).map(|v| v * h.radius);
        h.sofa.filter(x, y, z, &mut expected);
        for block in [128, 256, 512, 1024] {
            let mut b = Binaural::new(block, false)?;
            let mut input = vec![0.; block * (h.taps.div_ceil(block) + 1)];
            input[0] = 1.;
            let out = render(&mut b, &input, RIGHT)?;
            for n in 0..h.taps {
                assert!(
                    (out[n * 2] - expected.left[n]).abs() < 1e-5,
                    "block {block} L{n}"
                );
                assert!(
                    (out[n * 2 + 1] - expected.right[n]).abs() < 1e-5,
                    "block {block} R{n}"
                );
            }
            assert!(out[h.taps * 2..].iter().all(|v| v.abs() < 1e-5));
        }
        Ok(())
    }
    #[test]
    fn lateral_sources_favour_their_ear_and_front_is_balanced() -> Result<()> {
        let mut seed = 7;
        let input = noise(256 * 188, &mut seed);
        let db = |position| -> Result<f64> {
            let out = render(&mut Binaural::new(256, true)?, &input, position)?;
            Ok(20. * (rms(&out, 0) / rms(&out, 1)).log10())
        };
        assert!(db(LEFT)? > 10.);
        assert!(db(RIGHT)? < -10.);
        assert!(db(FRONT)?.abs() < 2.);
        Ok(())
    }
    #[test]
    fn direction_jump_crossfades_without_click() -> Result<()> {
        let block = 256;
        let mut b = Binaural::new(block, false)?;
        let tone: Vec<f32> = (0..block * 40)
            .map(|n| (2. * std::f32::consts::PI * 220. * n as f32 / RATE).sin() * 0.5)
            .collect();
        let mut out = render(&mut b, &tone[..block * 20], LEFT)?;
        out.extend(render(&mut b, &tone[block * 20..], RIGHT)?);
        let step = |range: std::ops::Range<usize>, channel: usize| {
            range
                .map(|n| (out[(n + 1) * 2 + channel] - out[n * 2 + channel]).abs())
                .fold(0f32, f32::max)
        };
        for channel in 0..2 {
            let steady =
                step(block * 4..block * 19, channel).max(step(block * 22..block * 39, channel));
            let switch = step(block * 19..block * 22, channel);
            assert!(
                switch <= steady * 1.5,
                "channel {channel}: {switch} > {steady}"
            );
        }
        Ok(())
    }
    #[test]
    fn drain_emits_tail_then_completes() -> Result<()> {
        for (block, calls) in [
            (256, vec![(255, true)]),
            (128, vec![(128, false), (127, true)]),
        ] {
            let mut b = Binaural::new(block, true)?;
            let mut seed = 3;
            render(&mut b, &noise(block * 4, &mut seed), RIGHT)?;
            let mut out = vec![0.; block * 2];
            for expected in calls {
                assert_eq!(b.drain(&mut out)?, expected);
            }
            assert_eq!(b.drain(&mut out)?, (0, true));
            assert!(b.process(&vec![0.; block], &mut out, RIGHT).is_err());
            b.reset();
            b.process(&vec![0.; block], &mut out, RIGHT)?;
        }
        let mut fresh = Binaural::new(256, true)?;
        assert_eq!(fresh.drain(&mut [0.; 512])?, (0, true));
        Ok(())
    }
    #[test]
    fn rejection_keeps_history() -> Result<()> {
        let mut a = Binaural::new(256, true)?;
        let mut b = Binaural::new(256, true)?;
        let mut input = [0.; 256];
        input[0] = 0.1;
        let (mut out, mut expected) = ([0.; 512], [0.; 512]);
        a.process(&input, &mut out, [1., 0., -1.])?;
        b.process(&input, &mut expected, [1., 0., -1.])?;
        assert_eq!(out, expected);
        let mut bad = input;
        bad[12] = f32::NAN;
        assert!(a.process(&bad, &mut out, [1., 0., -1.]).is_err());
        assert!(out.iter().all(|v| *v == 0.));
        assert!(a.process(&input, &mut out, [0., 0., 0.]).is_err());
        a.process(&input, &mut out, [1., 0., -1.])?;
        b.process(&input, &mut expected, [1., 0., -1.])?;
        assert_eq!(out, expected);
        Ok(())
    }
    #[test]
    fn reset_clears_history() -> Result<()> {
        let mut b = Binaural::new(256, true)?;
        let mut input = [0.; 256];
        input[255] = 1.;
        let mut out = [0.; 512];
        b.process(&input, &mut out, RIGHT)?;
        b.reset();
        b.process(&[0.; 256], &mut out, RIGHT)?;
        assert!(out.iter().all(|v| *v == 0.));
        Ok(())
    }
    #[test]
    fn processor_is_send() {
        fn send<T: Send>() {}
        send::<Binaural>();
    }
}
