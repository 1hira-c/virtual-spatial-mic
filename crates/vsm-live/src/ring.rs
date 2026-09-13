use crate::{capture::CapturedPacket, clock, queue::Queue};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use vsm_core::{
    processor::ProcessedBlock,
    wave::{WaveWriter, save_json},
};

pub type Status = Arc<dyn Fn(Value) + Send + Sync>;
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Audio,
    Event,
    Output,
    Checkpoint,
}
struct Item {
    kind: Kind,
    retained: i64,
    meta: Value,
    samples: Vec<f32>,
    bytes: usize,
}
impl Item {
    fn new(kind: Kind, retained: i64, meta: Value, samples: Vec<f32>) -> Self {
        let bytes = meta.to_string().len() + samples.len() * 4 + std::mem::size_of::<Self>();
        Self {
            kind,
            retained,
            meta,
            samples,
            bytes,
        }
    }
}
fn line(file: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}
fn extend(to: &mut Value, from: Value) {
    if let (Some(to), Value::Object(from)) = (to.as_object_mut(), from) {
        to.extend(from);
    }
}
struct Take {
    root: PathBuf,
    report: Value,
    dry: Option<WaveWriter>,
    output: WaveWriter,
    osc: Option<BufWriter<File>>,
    clock: Option<BufWriter<File>>,
    poses: Option<BufWriter<File>>,
    events: BufWriter<File>,
    raw_base: u64,
    raw_next: u64,
    sequence: u64,
    raw_started: bool,
    save_raw: bool,
    input_frames: u64,
    discontinuities: u64,
    bad_times: u64,
    gaps: u64,
    previous_device: u64,
    previous_valid: bool,
    first_audio: i64,
    last_audio: i64,
    last_flush: Instant,
}
impl Take {
    fn new(root: PathBuf, mut report: Value) -> Result<Self> {
        fs::create_dir(&root).with_context(|| {
            if root.exists() {
                "同じ録音開始日時の保存先が存在します。1秒以上あけて録音を開始してください。既存の録音は変更していません".to_owned()
            } else {
                format!("録音フォルダーを作成できません：{}", root.display())
            }
        })?;
        let output = WaveWriter::create(&root.join("live-output.wav"), 2)?;
        let events = BufWriter::new(File::create(root.join("obs.events.jsonl"))?);
        let save_raw = report["config"]["save_raw"].as_bool().unwrap_or(true);
        report["raw_data_saved"] = json!(save_raw);
        let (osc, clock, poses) = if save_raw {
            fs::create_dir(root.join("raw"))?;
            (
                Some(BufWriter::new(File::create(
                    root.join("raw/osc.events.jsonl"),
                )?)),
                Some(BufWriter::new(File::create(
                    root.join("raw/audio.clock.jsonl"),
                )?)),
                Some(BufWriter::new(File::create(
                    root.join("applied-poses.jsonl"),
                )?)),
            )
        } else {
            (None, None, None)
        };
        report["status"] = json!("recording");
        report["directory"] = json!(root);
        save_json(&root.join("session.json"), &report)?;
        Ok(Self {
            root,
            report,
            dry: None,
            output,
            osc,
            clock,
            poses,
            events,
            raw_base: 0,
            raw_next: 0,
            sequence: 0,
            raw_started: false,
            save_raw,
            input_frames: 0,
            discontinuities: 0,
            bad_times: 0,
            gaps: 0,
            previous_device: 0,
            previous_valid: false,
            first_audio: -1,
            last_audio: -1,
            last_flush: Instant::now(),
        })
    }
    fn mark(&mut self, row: &Value) -> Result<()> {
        line(&mut self.events, row)?;
        if row["video_path"].as_str().is_some_and(|s| !s.is_empty()) {
            self.report["video_path"] = row["video_path"].clone();
        }
        Ok(())
    }
    fn append(&mut self, item: &Item) -> Result<()> {
        let mut row = item.meta.clone();
        match item.kind {
            Kind::Checkpoint => {
                if self.save_raw && self.report.get("bootstrap").is_none() {
                    save_json(&self.root.join("raw/pose-bootstrap.json"), &row)?;
                    self.report["bootstrap"] = json!("raw/pose-bootstrap.json");
                }
            }
            Kind::Event => {
                if let Some(file) = &mut self.osc {
                    line(file, &row)?;
                }
            }
            Kind::Audio => {
                let channels = row["channels"].as_u64().context("Missing channels")? as u16;
                let flags = row["flags"].as_u64().context("Missing flags")?;
                let frames = row["frames"].as_u64().context("Missing frames")?;
                let engine_frame = row["engine_file_frame_start"]
                    .as_u64()
                    .context("Missing engine frame")?;
                if self.report["audio_format"]
                    .as_object()
                    .is_none_or(|f| f.is_empty())
                {
                    self.report["audio_format"] = row["audio_format"].clone();
                }
                if self.save_raw && self.dry.is_none() {
                    self.dry = Some(WaveWriter::create(
                        &self.root.join("raw/microphone.wav"),
                        channels,
                    )?);
                }
                if !self.raw_started {
                    self.raw_base = engine_frame;
                    self.raw_next = engine_frame;
                    self.raw_started = true;
                    self.first_audio = row["recorded_time_ns"].as_i64().unwrap_or(-1);
                }
                row["packet_sequence"] = json!(self.sequence);
                self.sequence += 1;
                row["file_frame_start"] = json!(self.input_frames);
                row["engine_frame_gap"] = json!(engine_frame != self.raw_next);
                self.raw_next = engine_frame + frames;
                let valid = row["timestamp_valid"].as_bool().unwrap_or(false);
                let device = row["device_position_frames"]
                    .as_u64()
                    .context("Missing device position")?;
                let gap = valid && self.previous_valid && device != self.previous_device;
                row["device_position_gap"] = json!(gap);
                self.discontinuities += u64::from(flags & 1 != 0);
                self.bad_times += u64::from(!valid);
                self.gaps += u64::from(gap);
                self.previous_device = device + frames;
                self.previous_valid = valid;
                self.last_audio = row["recorded_time_ns"].as_i64().unwrap_or(-1);
                self.input_frames += frames;
                if let Some(dry) = &mut self.dry {
                    dry.append(&item.samples)?;
                    line(self.clock.as_mut().unwrap(), &row)?;
                }
            }
            Kind::Output => {
                let first = row["input_file_frame"]
                    .as_u64()
                    .context("Missing source frame")?;
                if !self.raw_started || first < self.raw_base {
                    return Ok(());
                }
                row["engine_input_file_frame"] = json!(first);
                row["input_file_frame"] = json!(first - self.raw_base);
                row["engine_output_file_frame"] = row["output_file_frame"].clone();
                row["output_file_frame"] = json!(self.output.frames);
                self.output.append(&item.samples)?;
                if let Some(poses) = &mut self.poses {
                    line(poses, &row)?;
                }
            }
        }
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.flush()?;
            self.last_flush = Instant::now();
            ensure!(
                fs2::available_space(&self.root)? >= 256 * 1024 * 1024,
                "保存先の空き容量が256 MiB未満です"
            );
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if let Some(dry) = &mut self.dry {
            dry.flush()?;
        }
        self.output.flush()?;
        for file in [&mut self.osc, &mut self.clock, &mut self.poses]
            .into_iter()
            .flatten()
        {
            file.flush()?;
        }
        self.events.flush()?;
        Ok(())
    }
    fn finish(&mut self, end: Value) -> Result<Value> {
        extend(&mut self.report, end);
        self.report["raw_engine_frame_base"] = json!(self.raw_base);
        self.report["first_audio_time_ns"] = json!(self.first_audio);
        self.report["last_audio_time_ns"] = json!(self.last_audio);
        let start = self.report["recording_start_qpc_ns"].as_i64().unwrap_or(0)
            - self.report["session_origin_ns"].as_i64().unwrap_or(0);
        self.report["actual_preroll_seconds"] = json!(if self.first_audio >= 0 {
            ((start - self.first_audio) as f64 / 1e9).max(0.)
        } else {
            0.
        });
        self.report["statistics"] = json!({"raw_frames":self.dry.as_ref().map_or(0,|w|w.frames),"input_frames":self.input_frames,"output_frames":self.output.frames,"input_packets":self.sequence,"discontinuities":self.discontinuities,"timestamp_errors":self.bad_times,"device_position_gaps":self.gaps});
        if let Err(e) = self.flush() {
            self.report["status"] = json!("failed");
            self.report["error"] = json!(e.to_string());
        }
        save_json(&self.root.join("session.json"), &self.report)?;
        Ok(self.report.clone())
    }
}

enum Job {
    Begin {
        id: u64,
        meta: Value,
        seed: Vec<Arc<Item>>,
    },
    Item(u64, Arc<Item>),
    Mark(u64, Value),
    End(u64, Value),
}
struct Active {
    end_at: i64,
    end: Value,
}
pub struct Ring {
    config: Value,
    format: Value,
    origin: i64,
    horizon: i64,
    engine_id: String,
    status: Status,
    ring: VecDeque<Arc<Item>>,
    ring_bytes: usize,
    baseline: Option<Arc<Item>>,
    baseline_steps: Vec<Value>,
    baseline_bytes: usize,
    active: BTreeMap<u64, Active>,
    recording_id: u64,
    sequence: u64,
    dropped: u64,
    completed: u64,
    jobs: Arc<Queue<Job>>,
    failures: mpsc::Receiver<u64>,
    worker: Option<JoinHandle<()>>,
}
impl Ring {
    pub fn new(config: Value, origin: i64, status: Status) -> Result<Self> {
        let seconds = config["preroll_seconds"].as_f64().unwrap_or(10.);
        ensure!(
            seconds.is_finite() && (0.0..=30.0).contains(&seconds),
            "Invalid ring duration"
        );
        let engine_id = format!(
            "obs-{}-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            clock::ticks()
        );
        let jobs = Arc::new(Queue::new(128 * 1024 * 1024));
        let queue = jobs.clone();
        let cfg = config.clone();
        let callback = status.clone();
        let (failed, failures) = mpsc::channel();
        let worker = thread::spawn(move || Self::write(queue, cfg, callback, failed));
        Ok(Self {
            config,
            format: json!({}),
            origin,
            horizon: (seconds * 1e9) as i64,
            engine_id,
            status,
            ring: VecDeque::new(),
            ring_bytes: 0,
            baseline: None,
            baseline_steps: Vec::new(),
            baseline_bytes: 0,
            active: BTreeMap::new(),
            recording_id: 0,
            sequence: 0,
            dropped: 0,
            completed: 0,
            jobs,
            failures,
            worker: Some(worker),
        })
    }
    fn write(queue: Arc<Queue<Job>>, config: Value, status: Status, failures: mpsc::Sender<u64>) {
        let mut takes: BTreeMap<u64, Take> = BTreeMap::new();
        let mut failed = BTreeSet::new();
        loop {
            let Some(job) = queue.pop(Duration::from_millis(100)) else {
                if queue.drained() {
                    break;
                }
                continue;
            };
            let id = match &job {
                Job::Begin { id, .. } => *id,
                Job::Item(id, _) | Job::Mark(id, _) | Job::End(id, _) => *id,
            };
            let is_end = matches!(&job, Job::End(..));
            let result = (|| -> Result<()> {
                match job {
                    Job::Begin { id, meta, seed } => {
                        let root = Path::new(
                            config["output_root"]
                                .as_str()
                                .context("保存先を選んでください")?,
                        );
                        ensure!(!root.as_os_str().is_empty(), "保存先を選んでください");
                        fs::create_dir_all(root)?;
                        ensure!(
                            fs2::available_space(root)? >= 1024 * 1024 * 1024,
                            "保存開始には1 GiB以上の空き容量が必要です"
                        );
                        let name = meta["recording_name"]
                            .as_str()
                            .context("Missing recording name")?;
                        let path = root.join(name);
                        let mut take = Take::new(path.clone(), meta.clone())?;
                        take.mark(&meta["start_event"])?;
                        takes.insert(id, take);
                        for item in seed {
                            takes.get_mut(&id).unwrap().append(&item)?;
                        }
                        (status)(
                            json!({"archive_state":"recording","recording_directory":path,"archive_error":""}),
                        );
                    }
                    Job::Item(id, item) => {
                        if !failed.contains(&id)
                            && let Some(take) = takes.get_mut(&id)
                        {
                            take.append(&item)?;
                        }
                    }
                    Job::Mark(id, event) => {
                        if !failed.contains(&id)
                            && let Some(take) = takes.get_mut(&id)
                        {
                            take.mark(&event)?;
                        }
                    }
                    Job::End(id, end) => {
                        if !failed.contains(&id)
                            && let Some(take) = takes.get_mut(&id)
                        {
                            let report = take.finish(end)?;
                            (status)(
                                json!({"archive_state":if report["status"]=="failed"{"failed"}else{"saved"},"archive_error":report["error"].as_str().unwrap_or(""),"last_directory":report["directory"],"last_take_report":report}),
                            );
                            takes.remove(&id);
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                failed.insert(id);
                let _ = failures.send(id);
                let mut note = json!({"archive_state":"failed","archive_error":e.to_string()});
                if let Some(mut take) = takes.remove(&id) {
                    if let Ok(report)=take.finish(json!({"status":"failed","error":e.to_string(),"stop_reason":"archive_error"})){note["last_directory"]=report["directory"].clone();note["last_take_report"]=report;}
                }
                (status)(note);
            }
            if is_end {
                failed.remove(&id);
            }
        }
    }
    pub fn format(&mut self, value: Value) {
        self.format = value;
    }
    pub fn gain_locked(&self) -> bool {
        // The final 500 ms still belong to the take after Stop is pressed.
        !self.active.is_empty()
    }
    pub fn set_gain(&mut self, db: f64) {
        self.config["gain_db"] = json!(db);
    }
    fn push(&mut self, item: Arc<Item>) -> Result<()> {
        self.ring_bytes += item.bytes;
        self.ring.push_back(item.clone());
        while self.ring.front().is_some_and(|old| {
            old.retained < item.retained - self.horizon || self.ring_bytes > 64 * 1024 * 1024
        }) {
            let old = self.ring.pop_front().unwrap();
            if old.kind == Kind::Checkpoint {
                self.baseline = Some(old.clone());
                self.baseline_steps.clear();
                self.baseline_bytes = 0;
            } else if self.baseline.is_some() {
                let step = match old.kind {
                    Kind::Event => Some(json!({"kind":"event","event":old.meta})),
                    Kind::Output => Some(
                        json!({"kind":"pose_eval","source_time_ns":old.meta["source_time_ns"]}),
                    ),
                    _ => None,
                };
                if let Some(step) = step {
                    self.baseline_bytes += step.to_string().len();
                    ensure!(
                        self.baseline_bytes <= 8 * 1024 * 1024,
                        "Pose bootstrap history exceeds 8 MiB"
                    );
                    self.baseline_steps.push(step);
                }
            }
            if self.ring_bytes > 64 * 1024 * 1024 {
                self.dropped += 1;
            }
            self.ring_bytes -= old.bytes;
        }
        let mut failed = Vec::new();
        for id in self.active.keys() {
            if !self
                .jobs
                .push(Job::Item(*id, item.clone()), item.bytes, false)
            {
                self.jobs.push(Job::End(*id,json!({"status":"failed","error":"保存キューが上限に達しました","stop_reason":"archive_queue_overflow"})),0,true);
                (self.status)(
                    json!({"archive_state":"failed","archive_error":"保存キューが上限に達しました。モニターは継続しています"}),
                );
                failed.push(*id);
            }
        }
        for id in failed {
            if id == self.recording_id {
                self.recording_id = 0;
            }
            self.active.remove(&id);
        }
        Ok(())
    }
    pub fn audio(&mut self, p: &CapturedPacket, now: i64) -> Result<()> {
        let a = &p.audio;
        let samples = if self.config["save_raw"].as_bool().unwrap_or(true) {
            a.samples.clone()
        } else {
            Vec::new()
        };
        let row = json!({"engine_file_frame_start":a.file,"frames":a.frames,"channels":a.channels,"flags":a.flags,"device_position_frames":a.device,"qpc_position_100ns":p.qpc_100ns,"receive_qpc_ticks":p.receive_ticks,"recorded_time_ns":a.time,"timestamp_valid":a.flags&4==0,"silent":a.flags&2!=0,"audio_format":self.format});
        self.push(Arc::new(Item::new(Kind::Audio, now, row, samples)))
    }
    pub fn event(&mut self, event: &Value, now: i64) -> Result<()> {
        if self.config["save_raw"].as_bool().unwrap_or(true) {
            self.push(Arc::new(Item::new(
                Kind::Event,
                now,
                event.clone(),
                Vec::new(),
            )))?;
        }
        Ok(())
    }
    pub fn block(&mut self, block: &ProcessedBlock, now: i64) -> Result<()> {
        self.push(Arc::new(Item::new(
            Kind::Output,
            now,
            block.pose.clone(),
            block.samples.clone(),
        )))
    }
    pub fn checkpoint(&mut self, state: Value, now: i64) -> Result<()> {
        if self.config["save_raw"].as_bool().unwrap_or(true) {
            self.push(Arc::new(Item::new(
                Kind::Checkpoint,
                now,
                state,
                Vec::new(),
            )))?;
        }
        Ok(())
    }
    pub fn command(&mut self, event: Value) -> Result<()> {
        match event["event"].as_str().unwrap_or("") {
            "recording_started" => {
                if self.recording_id != 0 {
                    return Ok(());
                }
                let at = event["qpc_ns"]
                    .as_i64()
                    .context("Missing recording start time")?;
                // Use the Start event's local wall time, not engine startup or disk queue time.
                let started = chrono::Local::now()
                    - chrono::Duration::nanoseconds(clock::now_ns().saturating_sub(at));
                let name = started.format("%Y%m%d-%H%M%S").to_string();
                self.sequence += 1;
                let id = self.sequence;
                self.recording_id = id;
                self.active.insert(
                    id,
                    Active {
                        end_at: -1,
                        end: json!({}),
                    },
                );
                let meta = json!({"schema_version":"0.2.0","kind":"native_obs_ring_take","runtime":"rust","session_origin_ns":self.origin,"engine_id":self.engine_id,"recording_name":name,"recording_started_local":started.to_rfc3339(),"config":self.config,"audio_format":self.format,"recording_start_qpc_ns":at,"start_event":event,"requested_preroll_seconds":self.horizon as f64/1e9,"ring_evictions_by_capacity":self.dropped,"camera_policy":"hold_last_received_pose","wav_size_policy":"RIFF_then_RF64","python_runtime":false,"project_cpp_runtime":false,"delivery_delay_ns":300000000,"processing_wait_ns":100000000,"dsp_latency_frames":64,"offline_equivalent":false});
                let mut seed = Vec::new();
                if let Some(base) = &self.baseline {
                    let mut state = base.meta.clone();
                    state["replay_steps_before_ring"] = json!(self.baseline_steps);
                    seed.push(Arc::new(Item::new(
                        Kind::Checkpoint,
                        base.retained,
                        state,
                        Vec::new(),
                    )));
                }
                seed.extend(self.ring.iter().cloned());
                let cost = self.ring_bytes
                    + self.baseline_bytes
                    + self.baseline.as_ref().map_or(0, |b| b.bytes);
                if !self.jobs.push(Job::Begin { id, meta, seed }, cost, false) {
                    self.active.remove(&id);
                    self.recording_id = 0;
                    (self.status)(
                        json!({"archive_state":"failed","archive_error":"保存開始キューが上限に達しました"}),
                    );
                }
            }
            "recording_stopped" => {
                if self.recording_id == 0 {
                    return Ok(());
                }
                let id = self.recording_id;
                self.recording_id = 0;
                let active = self.active.get_mut(&id).unwrap();
                let at = event["qpc_ns"]
                    .as_i64()
                    .context("Missing recording stop time")?;
                active.end_at = at - self.origin + 500_000_000;
                active.end = json!({"status":"stopped","stop_reason":"obs_recording_stopped","recording_stop_qpc_ns":at,"video_path":event["video_path"].as_str().unwrap_or("")});
                let cost = event.to_string().len();
                self.jobs.push(Job::Mark(id, event), cost, false);
            }
            _ => {
                let cost = event.to_string().len();
                for id in self.active.keys() {
                    self.jobs.push(Job::Mark(*id, event.clone()), cost, false);
                }
            }
        }
        Ok(())
    }
    pub fn poll(&mut self, at: i64) {
        for id in self.failures.try_iter() {
            self.active.remove(&id);
            self.jobs
                .push(Job::End(id, json!({"status":"failed"})), 0, true);
            if self.recording_id == id {
                self.recording_id = 0;
            }
        }
        let finished: Vec<u64> = self
            .active
            .iter()
            .filter_map(|(&id, a)| (a.end_at >= 0 && at >= a.end_at).then_some(id))
            .collect();
        for id in finished {
            let a = self.active.remove(&id).unwrap();
            self.jobs.push(Job::End(id, a.end), 0, true);
            self.completed += 1;
        }
    }
    pub fn finish(&mut self, reason: &str) {
        for (id, mut a) in std::mem::take(&mut self.active) {
            extend(
                &mut a.end,
                json!({"status":if reason=="engine_error"{"failed"}else{"stopped"},"stop_reason":reason,"engine_end_qpc_ns":clock::now_ns()}),
            );
            self.jobs.push(Job::End(id, a.end), 0, true);
        }
        self.recording_id = 0;
        self.jobs.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    pub fn stats(&self) -> Value {
        json!({"ring_bytes":self.ring_bytes,"ring_limit_bytes":64*1024*1024,"ring_preroll_seconds":self.horizon as f64/1e9,"ring_capacity_evictions":self.dropped,"recording":self.recording_id!=0,"gain_locked":self.gain_locked(),"gain_db":self.config["gain_db"],"takes_started":self.sequence,"takes_completed":self.completed,"archive_queue_bytes":self.jobs.bytes()})
    }
}
impl Drop for Ring {
    fn drop(&mut self) {
        self.finish("engine_shutdown");
    }
}
