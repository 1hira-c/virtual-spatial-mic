//! Streaming playback of current sessions and historical prepared render plans.
//! Plans retain their interpolation contract; live-camera hold policy is owned
//! by Processor and is deliberately not applied to these solved timelines.
use crate::{
    dsp::Binaural,
    math::{self, Quat, Vec3},
    session,
    wave::{WaveReader, load_json},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
#[derive(Clone)]
struct Sample {
    at: i64,
    position: Vec3,
    rotation: Quat,
    frame: String,
    segment: String,
    valid: bool,
}
struct Timeline(Vec<Sample>);
impl Timeline {
    fn new(rows: &Value) -> Result<Self> {
        let rows = rows.as_array().context("Invalid preview timeline")?;
        ensure!(
            !rows.is_empty() && rows.len() <= 1_000_000,
            "Invalid preview timeline length"
        );
        let mut samples = Vec::new();
        let mut previous = -1;
        for r in rows {
            let s = Sample {
                at: r["effective_time_ns"]
                    .as_i64()
                    .context("Pose time must be integer")?,
                position: serde_json::from_value(r["position_m"].clone())?,
                rotation: serde_json::from_value(r["rotation_xyzw"].clone())?,
                frame: r["frame_id"].as_str().context("Missing frame")?.into(),
                segment: r["segment_id"].as_str().context("Missing segment")?.into(),
                valid: r["validity"] == "valid",
            };
            ensure!(
                s.at > previous && !s.frame.is_empty() && !s.segment.is_empty(),
                "Pose times must increase; frame and segment required"
            );
            ensure!(
                s.position.iter().all(|v| v.is_finite())
                    && s.rotation.iter().all(|v| v.is_finite())
                    && (s.rotation.iter().map(|v| v * v).sum::<f64>() - 1.).abs() <= 1e-5,
                "Invalid pose values"
            );
            previous = s.at;
            samples.push(s);
        }
        Ok(Self(samples))
    }
    fn at(&self, time: i64) -> Result<Sample> {
        let next = self.0.partition_point(|p| p.at < time);
        if let Some(p) = self.0.get(next).filter(|p| p.at == time) {
            ensure!(p.valid, "Invalid pose at requested time");
            return Ok(p.clone());
        }
        ensure!(
            next > 0 && next < self.0.len(),
            "Pose does not cover audio time"
        );
        let a = &self.0[next - 1];
        let b = &self.0[next];
        ensure!(
            a.valid
                && b.valid
                && a.frame == b.frame
                && a.segment == b.segment
                && b.at - a.at <= 250_000_000,
            "Invalid pose, frame/segment change, or long gap"
        );
        let t = (time - a.at) as f64 / (b.at - a.at) as f64;
        let mut result = a.clone();
        result.position = math::add(
            a.position,
            math::scale(math::sub(b.position, a.position), t),
        );
        result.rotation = slerp(a.rotation, b.rotation, t)?;
        Ok(result)
    }
}
fn slerp(a: Quat, b: Quat, t: f64) -> Result<Quat> {
    let a = math::unit(a)?;
    let mut b = math::unit(b)?;
    let mut dot = a.iter().zip(b).map(|(a, b)| a * b).sum::<f64>();
    if dot < 0. {
        b = b.map(|v| -v);
        dot = -dot;
    }
    dot = dot.clamp(-1., 1.);
    math::unit(if dot > 0.9995 {
        std::array::from_fn(|i| a[i] + t * (b[i] - a[i]))
    } else {
        let angle = dot.acos();
        std::array::from_fn(|i| {
            (((1. - t) * angle).sin() * a[i] + (t * angle).sin() * b[i]) / angle.sin()
        })
    })
}
struct Plan {
    audio: WaveReader,
    source: Timeline,
    listener: Timeline,
    processor: Binaural,
    block: usize,
    channel: usize,
    first: u64,
    buffer: Vec<f32>,
    read: usize,
    finished: bool,
    reference_sample: i64,
    reference_time: i64,
    step: f64,
}
impl Plan {
    fn new(path: &Path, library: &Path, cancel: &AtomicBool) -> Result<Self> {
        let plan = load_json(path)?;
        let audio_path = PathBuf::from(plan["audio_path"].as_str().context("Missing plan audio")?);
        let audio = WaveReader::open(&if audio_path.is_absolute() {
            audio_path
        } else {
            path.parent().unwrap_or(Path::new(".")).join(audio_path)
        })?;
        let channel = plan["channel"].as_u64().context("Invalid channel")? as usize;
        ensure!(
            channel < audio.channels as usize && audio.frames > 0,
            "Empty audio or invalid channel"
        );
        let block = plan["block_size"].as_u64().context("Invalid block size")? as usize;
        let distance = plan["distance_gain"]
            .as_str()
            .context("Missing distance gain")?;
        ensure!(
            ["none", "bounded_inverse"].contains(&distance),
            "Invalid distance gain"
        );
        let clock = &plan["clock_mapping"];
        let mut p = Self {
            audio,
            source: Timeline::new(&plan["source_poses"])?,
            listener: Timeline::new(&plan["listener_poses"])?,
            processor: Binaural::new(library, block, distance == "bounded_inverse")?,
            block,
            channel,
            first: 0,
            buffer: Vec::new(),
            read: 0,
            finished: false,
            reference_sample: clock["reference_sample"]
                .as_i64()
                .context("Invalid clock reference")?,
            reference_time: clock["time_at_reference_ns"]
                .as_i64()
                .context("Invalid clock time")?,
            step: clock["ns_per_sample"]
                .as_f64()
                .context("Invalid clock rate")?,
        };
        ensure!(
            p.step.is_finite() && p.step > 0.,
            "Invalid audio clock rate"
        );
        // Reject short invalid pose intervals even if they fall inside one DSP block.
        for first in (0..p.audio.frames).step_by(4096) {
            ensure!(!cancel.load(Ordering::Relaxed), "Preview cancelled");
            let n = (p.audio.frames - first).min(4096) as u32;
            ensure!(
                p.audio.read(first, n)?.iter().all(|v| v.is_finite()),
                "Nonfinite input"
            );
            for i in first..first + n as u64 {
                p.position(i)?;
            }
        }
        Ok(p)
    }
    fn position(&self, frame: u64) -> Result<Vec3> {
        let at =
            self.reference_time as f64 + (frame as f64 - self.reference_sample as f64) * self.step;
        ensure!(
            at.is_finite() && at >= 0. && at < (i64::MAX - 1) as f64,
            "Audio time outside session range"
        );
        let s = self.source.at(at.round() as i64)?;
        let l = self.listener.at(at.round() as i64)?;
        ensure!(
            s.frame == l.frame && s.segment == l.segment,
            "Source/listener frame mismatch"
        );
        let p = math::relative(s.position, l.position, math::unit(l.rotation)?);
        ensure!(math::norm(p) >= 1e-6, "Source coincides with listener");
        Ok(p)
    }
    fn read(&mut self, count: usize, cancel: &AtomicBool) -> Result<Vec<f32>> {
        let mut out = Vec::with_capacity(count * 2);
        while out.len() < count * 2 {
            ensure!(!cancel.load(Ordering::Relaxed), "Preview cancelled");
            if self.read == self.buffer.len() {
                if self.finished {
                    break;
                }
                self.buffer = vec![0.; self.block * 2];
                self.read = 0;
                if self.first < self.audio.frames {
                    let n = (self.audio.frames - self.first).min(self.block as u64) as u32;
                    let input = self.audio.read(self.first, n)?;
                    let mut mono = vec![0.; self.block];
                    for (i, frame) in input.chunks_exact(self.audio.channels as usize).enumerate() {
                        mono[i] = frame[self.channel];
                    }
                    let pos = self.position(self.first)?;
                    self.processor.process(&mono, &mut self.buffer, pos)?;
                    self.first += n as u64;
                } else {
                    let (n, done) = self.processor.drain(&mut self.buffer)?;
                    self.buffer.truncate(n * 2);
                    self.finished = done;
                    if n == 0 {
                        break;
                    }
                }
            }
            let n = (count * 2 - out.len()).min(self.buffer.len() - self.read);
            out.extend_from_slice(&self.buffer[self.read..self.read + n]);
            self.read += n;
        }
        Ok(out)
    }
}
enum Stream {
    Wave(WaveReader, u64),
    Plan(Box<Plan>),
}
pub struct RecordingStream {
    inner: Stream,
    frames: u64,
    leading: usize,
    trailing: usize,
}
impl RecordingStream {
    pub fn open(
        selection: &Path,
        boundary: usize,
        cancel: &AtomicBool,
        library: &Path,
    ) -> Result<Self> {
        ensure!(boundary <= 48000, "Preview guard exceeds one second");
        let (inner, frames) = if selection.is_dir()
            && (selection.join("session.json").is_file() || selection.join("take.json").is_file())
        {
            let info = session::inspect(selection)?;
            let path = if info["can_reprocess"] == true {
                let report = session::reprocess(
                    selection,
                    &json!({"gain_db":0.}),
                    cancel,
                    library,
                    |_| {},
                    false,
                )?;
                PathBuf::from(report["output_path"].as_str().unwrap())
            } else {
                session::audio_path(selection, false)?
            };
            let reader = WaveReader::open(&path)?;
            ensure!(reader.channels == 2, "Recorded playback requires stereo");
            let frames = reader.frames;
            (Stream::Wave(reader, 0), frames)
        } else {
            let path = if selection.is_dir() {
                let renders = selection.join("renders").canonicalize()?;
                let latest = load_json(&renders.join("latest-preview.json"))?;
                let output = PathBuf::from(
                    latest["output"]
                        .as_str()
                        .context("Invalid saved preview path")?,
                )
                .canonicalize()?;
                ensure!(
                    output.starts_with(&renders),
                    "Saved preview escapes recording"
                );
                output.parent().unwrap().join("render-plan.json")
            } else {
                selection.into()
            };
            let plan = Plan::new(&path, library, cancel)?;
            let frames = plan.audio.frames;
            (Stream::Plan(Box::new(plan)), frames)
        };
        Ok(Self {
            inner,
            frames,
            leading: boundary,
            trailing: boundary,
        })
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    pub fn read(&mut self, count: usize, cancel: &AtomicBool) -> Result<Vec<f32>> {
        ensure!(count <= 480000, "Playback read limit");
        ensure!(!cancel.load(Ordering::Relaxed), "Preview cancelled");
        let n = count.min(self.leading);
        self.leading -= n;
        let mut out = vec![0.; n * 2];
        if n < count {
            let pcm = match &mut self.inner {
                Stream::Wave(w, first) => {
                    let n = (w.frames - *first).min((count - n) as u64) as u32;
                    let pcm = w.read(*first, n)?;
                    *first += n as u64;
                    pcm
                }
                Stream::Plan(p) => p.read(count - n, cancel)?,
            };
            out.extend(pcm);
            let remain = (count - out.len() / 2).min(self.trailing);
            self.trailing -= remain;
            out.resize(out.len() + remain * 2, 0.);
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_interpolation_across_invalid_interval() -> Result<()> {
        let row = |time, valid| json!({"effective_time_ns":time,"frame_id":"world","segment_id":"one","position_m":[1.,0.,0.],"rotation_xyzw":[0.,0.,0.,1.],"validity":valid});
        let timeline = Timeline::new(&json!([
            row(0, "valid"),
            row(100, "invalid"),
            row(200, "valid")
        ]))?;
        assert!(timeline.at(50).is_err());
        assert!(timeline.at(100).is_err());
        assert!(timeline.at(0).is_ok());
        Ok(())
    }
}
