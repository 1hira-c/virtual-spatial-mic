use crate::{
    dsp::{Binaural, Level},
    osc::{Message, decode_osc, live_types},
    pose::LivePose,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub samples: Vec<f32>,
    pub frames: u32,
    pub channels: u32,
    pub flags: u32,
    pub device: u64,
    pub file: u64,
    pub time: i64,
}
pub struct ProcessedBlock {
    pub samples: Vec<f32>,
    pub pose: Value,
    pub timestamp: i64,
    pub late: bool,
}
pub struct Processor {
    pose: LivePose,
    dsp: Binaural,
    level: Level,
    events: BTreeMap<(i64, u64), Value>,
    sequence: u64,
    origin: i64,
    clock: Option<fn() -> i64>,
    gain: f64,
    applied_gain: f64,
    gain_override: bool,
    packet_time: i64,
    packet_frame: u64,
    last_source: i64,
    mono: [f32; 256],
    used: usize,
    next_frame: u64,
    channels: usize,
    channel: usize,
    started: bool,
    finished: bool,
    expected_device: u64,
    previous: bool,
    was_valid: bool,
    previous_model: String,
    out_frames: u64,
    muted: u64,
    late: u64,
    discontinuities: u64,
    discarded: u64,
    source_frames: BTreeMap<String, u64>,
}
impl Processor {
    pub fn new(
        config: &Value,
        origin: i64,
        library: &Path,
        clock: Option<fn() -> i64>,
    ) -> Result<Self> {
        let db = config
            .get("gain_db")
            .and_then(Value::as_f64)
            .unwrap_or(-18.);
        ensure!(
            db.is_finite() && (-60.0..=0.0).contains(&db),
            "Invalid gain"
        );
        let offset = serde_json::from_value(
            config
                .get("mouth_offset_m")
                .cloned()
                .unwrap_or(json!([0., 0.0064, -0.0736])),
        )?;
        let mut dsp = Binaural::new(library, 256, true)?;
        let mut stereo = [0.; 512];
        dsp.process(&[0.; 256], &mut stereo, [0., 0., -1.])?;
        dsp.reset();
        Ok(Self {
            pose: LivePose::new(
                offset,
                config
                    .get("source_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("auto"),
            )?,
            dsp,
            level: Level::default(),
            events: BTreeMap::new(),
            sequence: 0,
            origin,
            clock,
            gain: 10f64.powf(db / 20.),
            applied_gain: 10f64.powf(db / 20.),
            gain_override: false,
            packet_time: 0,
            packet_frame: 0,
            last_source: -1,
            mono: [0.; 256],
            used: 0,
            next_frame: 0,
            channels: 0,
            channel: 0,
            started: false,
            finished: false,
            expected_device: 0,
            previous: false,
            was_valid: false,
            previous_model: String::new(),
            out_frames: 0,
            muted: 0,
            late: 0,
            discontinuities: 0,
            discarded: 0,
            source_frames: BTreeMap::new(),
        })
    }
    fn messages(row: &Value) -> Result<Vec<Message>> {
        match row.get("kind").and_then(Value::as_str).unwrap_or("") {
            "udp" => decode_osc(
                &STANDARD.decode(
                    row["datagram_base64"]
                        .as_str()
                        .context("Missing OSC datagram")?,
                )?,
            ),
            "snapshot" => {
                let body: Value =
                    serde_json::from_str(row["body"].as_str().context("Missing HTTP body")?)?;
                let address = row["address"]
                    .as_str()
                    .context("Missing snapshot address")?;
                let types = live_types();
                let kind = types.get(address).context("Unknown OSC address")?;
                ensure!(
                    body["FULL_PATH"].as_str() == Some(address)
                        && body["TYPE"].as_str() == Some(kind)
                        && body["ACCESS"].as_u64().unwrap_or(0) & 1 != 0,
                    "Invalid OSCQuery response"
                );
                let values = body["VALUE"]
                    .as_array()
                    .context("Missing OSCQuery values")?;
                ensure!(values.len() == 1, "Invalid scalar response");
                let tag = if kind == "T" {
                    if values[0].as_bool().context("Invalid boolean")? {
                        ",T"
                    } else {
                        ",F"
                    }
                    .to_owned()
                } else {
                    ensure!(
                        values[0].as_f64().is_some_and(f64::is_finite),
                        "Invalid scalar"
                    );
                    format!(",{kind}")
                };
                Ok(vec![Message {
                    address: address.into(),
                    typetag: tag,
                    values: values.clone(),
                }])
            }
            _ => anyhow::bail!("Not a pose event"),
        }
    }
    pub fn event(&mut self, mut row: Value) -> Result<()> {
        ensure!(self.events.len() < 200_000, "OSC timeline exceeded bounds");
        let at = row["receive_time_ns"]
            .as_i64()
            .context("Missing receipt time")?;
        if row["kind"] == "gain_change" {
            let db = row["gain_db"].as_f64().context("Missing gain")?;
            ensure!(
                db.is_finite() && (-60.0..=0.0).contains(&db),
                "Invalid gain"
            );
        } else {
            let Ok(messages) = Self::messages(&row) else {
                return Ok(());
            };
            row["messages"] = serde_json::to_value(messages)?;
        }
        self.events
            .insert((at.max(self.last_source + 1), self.sequence), row);
        self.sequence += 1;
        Ok(())
    }
    fn position(&mut self, at: i64) -> Result<crate::pose::PoseResult> {
        self.last_source = at;
        while self
            .events
            .first_key_value()
            .is_some_and(|(k, _)| k.0 <= at)
        {
            let (_, item) = self.events.pop_first().unwrap();
            if item["kind"] == "gain_change" {
                if !self.gain_override {
                    self.gain = 10f64.powf(item["gain_db"].as_f64().context("Missing gain")? / 20.);
                }
                continue;
            }
            let messages = serde_json::from_value::<Vec<Message>>(item["messages"].clone())?;
            self.pose.feed(
                &messages,
                item["receive_time_ns"]
                    .as_i64()
                    .context("Missing receipt time")?,
                item["kind"] == "snapshot",
                item["request_start_ns"].as_i64().unwrap_or(0),
            )?;
        }
        self.pose.at(at)
    }
    fn block(&mut self, first: u64, valid: usize) -> Result<ProcessedBlock> {
        let delta = ((first as i128 - self.packet_frame as i128) * 1_000_000_000 / 48000) as i64;
        let at = (self.packet_time + delta).max(self.last_source + 1);
        let mut p = self.position(at)?;
        let mut stereo = vec![0.; 512];
        let mut ready = p.valid && self.mono.iter().all(|v| v.is_finite());
        if ready {
            if !self.was_valid || self.previous_model != p.source_model {
                self.dsp.reset();
            }
            if self
                .dsp
                .process(&self.mono, &mut stereo, p.relative_m)
                .is_err()
            {
                ready = false;
                p.reason = "音声処理が入力を拒否しました".into();
            }
        }
        if !ready {
            stereo.fill(0.);
            self.dsp.reset();
            self.muted += valid as u64;
        }
        self.was_valid = ready;
        self.previous_model = p.source_model.clone();
        *self
            .source_frames
            .entry(p.source_model.clone())
            .or_default() += valid as u64;
        let gain_from = self.applied_gain;
        self.level.apply_ramp(&mut stereo, gain_from, self.gain)?;
        self.applied_gain = self.gain;
        let timestamp = self.origin + at + 300_000_000;
        let late = self
            .clock
            .is_some_and(|clock| clock() > timestamp - 25_000_000);
        if late {
            self.late += 1;
            stereo.fill(0.);
        }
        let pose = json!({"input_file_frame":first,"output_file_frame":self.out_frames,"valid_frames":valid,"source_time_ns":at,"obs_timestamp_ns":timestamp,"valid":ready,"reason":p.reason,"source_model":p.source_model,"late_delivery":late,"relative_m":if ready{json!(p.relative_m)}else{Value::Null},"mouth_m":if ready{json!(p.mouth_m)}else{Value::Null},"camera_m":if p.valid{json!(p.camera.position)}else{Value::Null},"orientation":p.orientation,"details":p.details});
        let mut pose = pose;
        pose["gain_db"] = json!(20. * self.gain.log10());
        pose["gain_from_linear"] = json!(gain_from);
        self.out_frames += valid as u64;
        stereo.truncate(valid * 2);
        Ok(ProcessedBlock {
            samples: stereo,
            pose,
            timestamp,
            late,
        })
    }
    fn reset_packet(&mut self, first: u64) {
        self.discarded += self.used as u64;
        self.used = 0;
        self.next_frame = first;
    }
    pub fn push(&mut self, a: &AudioPacket, channel: usize) -> Result<Vec<ProcessedBlock>> {
        ensure!(!self.finished, "Processor already finished");
        ensure!(
            a.channels > 0 && a.channels <= 32 && channel < a.channels as usize,
            "Selected microphone channel is unavailable"
        );
        ensure!(
            a.frames <= 480000 && a.samples.len() == a.frames as usize * a.channels as usize,
            "Invalid audio packet length"
        );
        if !self.started {
            self.started = true;
            self.next_frame = a.file;
            self.channels = a.channels as usize;
            self.channel = channel;
        }
        ensure!(
            self.channels == a.channels as usize && self.channel == channel,
            "Audio format changed"
        );
        let invalid_time = a.flags & 4 != 0;
        if a.flags & 1 != 0 || invalid_time || (self.previous && a.device != self.expected_device) {
            self.reset_packet(a.file);
            self.dsp.reset();
            self.was_valid = false;
            self.discontinuities += 1;
        }
        self.previous = !invalid_time;
        self.expected_device = a.device + a.frames as u64;
        if invalid_time {
            self.discarded += a.frames as u64;
            self.reset_packet(a.file + a.frames as u64);
            return Ok(Vec::new());
        }
        ensure!(a.file == self.next_frame, "Audio file frame discontinuity");
        self.packet_time = a.time;
        self.packet_frame = a.file;
        let mut blocks = Vec::new();
        let mut consumed = 0;
        while consumed < a.frames as usize {
            let count = (a.frames as usize - consumed).min(256 - self.used);
            for i in 0..count {
                self.mono[self.used + i] = a.samples[(consumed + i) * self.channels + self.channel];
            }
            consumed += count;
            self.used += count;
            self.next_frame += count as u64;
            if self.used == 256 {
                self.used = 0;
                blocks.push(self.block(self.next_frame - 256, 256)?);
            }
        }
        Ok(blocks)
    }
    pub fn finish(&mut self) -> Result<Vec<ProcessedBlock>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        if self.used > 0 {
            let valid = self.used;
            self.mono[valid..].fill(0.);
            self.used = 0;
            return Ok(vec![self.block(self.next_frame - valid as u64, valid)?]);
        }
        Ok(Vec::new())
    }
    pub fn statistics(&self) -> Value {
        json!({"output_frames":self.out_frames,"muted_frames":self.muted,"late_blocks":self.late,"discontinuities":self.discontinuities,"discarded_partial_or_bad_timestamp_frames":self.discarded,"limited_frames":self.level.limited_frames,"source_model_frames":self.source_frames})
    }
    pub fn checkpoint(&self) -> Value {
        let pending: Vec<Value> = self
            .events
            .iter()
            .map(|(key, row)| json!({"effective_time_ns":key.0,"event":row}))
            .collect();
        json!({"kind":"derived_processor_checkpoint","pose":self.pose.checkpoint(),"pending_events":pending,"last_source_time_ns":self.last_source,"engine_output_frames":self.out_frames,"dsp_state_serialized":false,"gain_db":20.*self.gain.log10(),"applied_gain":self.applied_gain})
    }
    /// An explicit reprocessing level replaces the recorded gain timeline.
    pub fn override_gain(&mut self, db: f64) -> Result<()> {
        ensure!(
            db.is_finite() && (-60.0..=0.0).contains(&db),
            "Invalid gain"
        );
        self.gain = 10f64.powf(db / 20.);
        self.applied_gain = self.gain;
        self.gain_override = true;
        Ok(())
    }
    pub fn restore(&mut self, state: &Value) -> Result<()> {
        ensure!(
            !self.started && state["kind"] == "derived_processor_checkpoint",
            "Invalid processor checkpoint"
        );
        self.pose.restore(&state["pose"])?;
        if !self.gain_override {
            if let Some(db) = state["gain_db"].as_f64() {
                ensure!(
                    db.is_finite() && (-60.000001..=0.0).contains(&db),
                    "Invalid checkpoint gain"
                );
                self.gain = 10f64.powf(db / 20.);
                let applied = state["applied_gain"].as_f64().unwrap_or(self.gain);
                ensure!(
                    applied.is_finite() && (0.0..=1.0).contains(&applied),
                    "Invalid applied gain"
                );
                self.applied_gain = applied;
            }
        }
        self.last_source = state["last_source_time_ns"]
            .as_i64()
            .context("Missing checkpoint time")?;
        let pending = state["pending_events"]
            .as_array()
            .context("Invalid checkpoint events")?;
        ensure!(pending.len() <= 200_000, "Checkpoint events exceed limit");
        for entry in pending {
            let t = entry["effective_time_ns"]
                .as_i64()
                .context("Invalid event time")?;
            self.events
                .insert((t, self.sequence), entry["event"].clone());
            self.sequence += 1;
        }
        if let Some(steps) = state.get("replay_steps_before_ring") {
            let steps = steps.as_array().context("Invalid bootstrap history")?;
            ensure!(steps.len() <= 200_000, "Bootstrap history exceeds limit");
            for step in steps {
                match step["kind"].as_str() {
                    Some("event") => self.event(step["event"].clone())?,
                    Some("pose_eval") => {
                        let at = step["source_time_ns"]
                            .as_i64()
                            .context("Invalid evaluation time")?;
                        if at > self.last_source {
                            self.position(at)?;
                        }
                    }
                    _ => anyhow::bail!("Unknown bootstrap step"),
                }
            }
        }
        Ok(())
    }
}
