use crate::{
    capture::{self, CapturedPacket},
    clock,
    meter::Levels,
    osc::OscLive,
    queue::Queue,
    ring::{Ring, Status},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use vsm_core::{
    processor::{ProcessedBlock, Processor},
    session::Lines,
};

pub type AudioOutput = Arc<dyn Fn(&[f32], i64) -> Result<()> + Send + Sync>;
#[derive(Default)]
struct GainReply {
    cancelled: bool,
    result: Option<std::result::Result<(), String>>,
}
enum Control {
    Event(Value),
    Gain {
        db: f64,
        reply: Arc<Mutex<GainReply>>,
        wake: SyncSender<()>,
    },
}
enum CaptureEvent {
    Format(Value),
    Packet(CapturedPacket),
    Done(Result<Value>),
}
struct CaptureWorker {
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl CaptureWorker {
    fn join(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Capture worker failed"))?;
        }
        Ok(())
    }
}
impl Drop for CaptureWorker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}
pub struct Engine {
    cancel: Arc<AtomicBool>,
    commands: Arc<Queue<Control>>,
    state: Arc<Mutex<Value>>,
    levels: Arc<Levels>,
    worker: Option<JoinHandle<()>>,
}
fn update(state: &Mutex<Value>, value: Value) {
    if let Value::Object(value) = value {
        state.lock().unwrap().as_object_mut().unwrap().extend(value);
    }
}
impl Engine {
    pub fn start(config: Value, library: PathBuf, output: AudioOutput) -> Result<Self> {
        vsm_core::session::settings(&config)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let commands = Arc::new(Queue::new(1024 * 1024));
        let state = Arc::new(Mutex::new(
            json!({"state":"initializing","message":"入力を準備しています","recording":false,"active":true}),
        ));
        let stop = cancel.clone();
        let controls = commands.clone();
        let info = state.clone();
        let levels = Arc::new(Levels::default());
        let input_levels = levels.clone();
        let output_levels = levels.clone();
        let output: AudioOutput = Arc::new(move |samples, at| {
            output(samples, at)?;
            output_levels.output(samples, clock::now_ns());
            Ok(())
        });
        let worker=thread::Builder::new().name("vsm-engine".into()).spawn(move||{
            let status:Status={let info=info.clone();Arc::new(move|value|update(&info,value))};
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||run(&config,&library,stop,controls,output,status,input_levels)));
            match result{Ok(Ok(report))=>update(&info,json!({"active":false,"recording":false,"state":report["status"],"message":if report["status"]=="failed"{format!("入力停止：{}",report["error"].as_str().unwrap_or("unknown"))}else{"入力は無効です".into()},"engine_report":report})),Ok(Err(e))=>update(&info,json!({"active":false,"recording":false,"state":"failed","message":e.to_string()})),Err(_)=>update(&info,json!({"active":false,"recording":false,"state":"failed","message":"音声処理スレッドが異常終了しました"}))}
        })?;
        Ok(Self {
            cancel,
            commands,
            state,
            levels,
            worker: Some(worker),
        })
    }
    pub fn command(&self, mut event: Value) -> Result<()> {
        if event.get("qpc_ns").is_none() {
            event["qpc_ns"] = json!(clock::now_ns());
        }
        let bytes = event.to_string().len();
        ensure!(
            self.commands.push(Control::Event(event), bytes, false),
            "録音操作キューが上限に達しました"
        );
        Ok(())
    }
    pub fn set_gain(&self, db: f64) -> Result<()> {
        ensure!(
            db.is_finite() && (-60.0..=0.0).contains(&db),
            "Invalid gain"
        );
        ensure!(
            !self.finished() && !self.cancel.load(Ordering::Relaxed),
            "入力が停止しています"
        );
        let reply = Arc::new(Mutex::new(GainReply::default()));
        let (wake, done) = mpsc::sync_channel(1);
        ensure!(
            self.commands.push(
                Control::Gain {
                    db,
                    reply: reply.clone(),
                    wake
                },
                128,
                false
            ),
            "設定キューが上限に達しました"
        );
        let _ = done.recv_timeout(Duration::from_secs(2));
        let mut reply = reply.lock().unwrap();
        if let Some(result) = reply.result.take() {
            return result.map_err(anyhow::Error::msg);
        }
        // Cancellation and application share this lock, so a timeout cannot
        // leave a gain change queued to apply after reporting failure.
        reply.cancelled = true;
        anyhow::bail!("ゲイン変更に応答がありません。入力の状態を確認してください")
    }
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn status(&self) -> Value {
        self.state.lock().unwrap().clone()
    }
    pub fn audio_levels(&self) -> [f32; 3] {
        let peaks = self.levels.take(clock::now_ns());
        if self.finished() || self.cancel.load(Ordering::Relaxed) {
            [0.; 3]
        } else {
            peaks
        }
    }
    /// Snapshot channel for native host status observers. Never hold its lock
    /// while calling host APIs or stopping the engine.
    pub fn shared_status(&self) -> Arc<Mutex<Value>> {
        self.state.clone()
    }
    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn join(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.join();
    }
}

fn deliver(
    blocks: Vec<ProcessedBlock>,
    ring: &mut Ring,
    output: &AudioOutput,
    origin: i64,
    last_pose: &mut Value,
) -> Result<()> {
    for block in blocks {
        if !block.late {
            output(&block.samples, block.timestamp)?;
        }
        *last_pose = block.pose.clone();
        ring.block(&block, clock::now_ns() - origin)?;
    }
    Ok(())
}
fn control(
    command: Control,
    ring: &mut Ring,
    processor: &mut Processor,
    status: &Status,
    origin: i64,
    live: bool,
) -> Result<()> {
    match command {
        Control::Event(event) => ring.command(event)?,
        Control::Gain { db, reply, wake } => {
            let mut reply = reply.lock().unwrap();
            if reply.cancelled {
                return Ok(());
            }
            if !live || ring.gain_locked() {
                reply.result = Some(Err("録音中・録音終了処理中はゲインを変更できません".into()));
                let _ = wake.try_send(());
                return Ok(());
            }
            let result = (|| -> Result<()> {
                let now = clock::now_ns() - origin;
                let row = json!({"kind":"gain_change","receive_time_ns":now,"gain_db":db,"source":"studio"});
                processor.event(row.clone())?;
                ring.event(&row, now)?;
                ring.set_gain(db);
                status(ring.stats());
                Ok(())
            })();
            reply.result = Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
            let _ = wake.try_send(());
            result?;
        }
    }
    status(ring.stats());
    Ok(())
}
fn run(
    config: &Value,
    library: &Path,
    cancel: Arc<AtomicBool>,
    commands: Arc<Queue<Control>>,
    output: AudioOutput,
    status: Status,
    levels: Arc<Levels>,
) -> Result<Value> {
    let origin = clock::now_ns();
    let mut ring = Ring::new(config.clone(), origin, status.clone())?;
    let mut processor = Processor::new(config, origin, library, Some(clock::now_ns))?;
    let audio = Arc::new(Queue::<CaptureEvent>::new(32 * 1024 * 1024));
    let events = Arc::new(Queue::<Value>::new(16 * 1024 * 1024));
    let mut fixtures = VecDeque::new();
    let mut osc = None;
    let mut capture = CaptureWorker {
        cancel: cancel.clone(),
        worker: None,
    };
    let mut pending = None;
    let mut report = json!({"kind":"native_obs_ring_engine","runtime":"rust","session_origin_ns":origin,"memory_only_while_idle":true,"python_runtime":false,"project_cpp_runtime":false});
    let mut last_pose = json!({"reason":"位置データ待ち","valid":false});
    let result = (|| -> Result<()> {
        if let Some(path) = config["fixture_events_path"].as_str() {
            let mut lines = Lines::open(Path::new(path))?;
            while let Some(row) = lines.next.take() {
                ensure!(fixtures.len() < 200_000, "Fixture event limit");
                fixtures.push_back(row);
                lines.advance()?;
            }
        } else {
            let queue = events.clone();
            osc = Some(OscLive::new(
                origin,
                Arc::new(move |row| {
                    let bytes = row.to_string().len();
                    ensure!(
                        queue.push(row, bytes, false),
                        "OSC event queue full; input stopped"
                    );
                    Ok(())
                }),
            )?);
        }
        report["network"] = osc
            .as_ref()
            .map(OscLive::status)
            .unwrap_or(json!({"fixture_events_only":true}));
        let cfg = config.clone();
        let stop = cancel.clone();
        let queue = audio.clone();
        capture.worker = Some(thread::Builder::new().name("vsm-capture".into()).spawn(
            move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    capture::capture(
                        &cfg,
                        origin,
                        &stop,
                        |fmt| {
                            ensure!(
                                queue.push(CaptureEvent::Format(fmt), 1024, false),
                                "Audio format queue full"
                            );
                            Ok(())
                        },
                        |packet| {
                            levels.input(
                                &packet.audio.samples,
                                packet.audio.channels as usize,
                                cfg["channel"].as_u64().unwrap_or(0) as usize,
                                clock::now_ns(),
                            );
                            let bytes = packet.audio.samples.len() * 4 + 256;
                            ensure!(
                                queue.push(CaptureEvent::Packet(packet), bytes, false),
                                "Live processing queue full; input stopped"
                            );
                            Ok(())
                        },
                    )
                }));
                queue.push(
                    CaptureEvent::Done(match result {
                        Ok(v) => v,
                        Err(_) => Err(anyhow::anyhow!("Capture thread panicked")),
                    }),
                    0,
                    true,
                );
                queue.close();
            },
        )?);
        let mut capture_done = false;
        let mut last_checkpoint = -1_000_000_000;
        let mut last_status = -250_000_000;
        loop {
            let now = clock::now_ns() - origin;
            while let Some(command) = commands.pop(Duration::ZERO) {
                control(
                    command,
                    &mut ring,
                    &mut processor,
                    &status,
                    origin,
                    !cancel.load(Ordering::Relaxed),
                )?;
            }
            while let Some(event) = events.pop(Duration::ZERO) {
                ring.event(&event, now)?;
                processor.event(event)?;
            }
            while fixtures
                .front()
                .is_some_and(|e| e["receive_time_ns"].as_i64().is_some_and(|t| t <= now))
            {
                let event = fixtures.pop_front().unwrap();
                ring.event(&event, now)?;
                processor.event(event)?;
            }
            if let Some(network) = &osc {
                let info = network.status();
                ensure!(
                    info["error"].as_str().unwrap_or("").is_empty(),
                    "{}",
                    info["error"]
                );
            }
            if pending.is_none() {
                if let Some(event) = audio.pop(Duration::from_millis(5)) {
                    match event {
                        CaptureEvent::Format(fmt) => ring.format(fmt),
                        CaptureEvent::Packet(packet) => pending = Some(packet),
                        CaptureEvent::Done(result) => {
                            report["audio_capture"] = result?;
                            capture_done = true;
                        }
                    }
                }
            }
            if pending.as_ref().is_some_and(|p: &CapturedPacket| {
                cancel.load(Ordering::Relaxed) || p.audio.time + 100_000_000 <= now
            }) {
                let packet = pending.take().unwrap();
                ring.audio(&packet, now)?;
                deliver(
                    processor.push(
                        &packet.audio,
                        config["channel"].as_u64().unwrap_or(0) as usize,
                    )?,
                    &mut ring,
                    &output,
                    origin,
                    &mut last_pose,
                )?;
            }
            if now - last_checkpoint >= 1_000_000_000 {
                ring.checkpoint(processor.checkpoint(), now)?;
                last_checkpoint = now;
            }
            ring.poll(now);
            if now - last_status >= 250_000_000 {
                let mut info = ring.stats();
                info["state"] = json!("running");
                info["message"] = last_pose["reason"].clone();
                info["pose_ready"] = last_pose["valid"].clone();
                info["statistics"] = processor.statistics();
                info["active"] = json!(true);
                info["network"] = osc
                    .as_ref()
                    .map(OscLive::status)
                    .unwrap_or(json!({"fixture_events_only":true}));
                status(info);
                last_status = now;
            }
            if capture_done && pending.is_none() {
                break;
            }
            if pending.is_some() {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    })();
    cancel.store(true, Ordering::Relaxed);
    capture.join()?;
    drop(osc);
    // Retain already acquired packets even if a network/control error stops the
    // engine. Every accepted packet keeps its original device/QPC clock.
    let mut final_error = result.err();
    if let Some(packet) = pending.take() {
        let result = (|| {
            ring.audio(&packet, clock::now_ns() - origin)?;
            deliver(
                processor.push(
                    &packet.audio,
                    config["channel"].as_u64().unwrap_or(0) as usize,
                )?,
                &mut ring,
                &output,
                origin,
                &mut last_pose,
            )
        })();
        if let Err(e) = result {
            final_error.get_or_insert(e);
        }
    }
    while let Some(event) = events.pop(Duration::ZERO) {
        if let Err(e) = ring
            .event(&event, clock::now_ns() - origin)
            .and_then(|_| processor.event(event))
        {
            final_error.get_or_insert(e);
        }
    }
    while let Some(event) = audio.pop(Duration::ZERO) {
        match event {
            CaptureEvent::Packet(packet) => {
                let result = (|| {
                    ring.audio(&packet, clock::now_ns() - origin)?;
                    deliver(
                        processor.push(
                            &packet.audio,
                            config["channel"].as_u64().unwrap_or(0) as usize,
                        )?,
                        &mut ring,
                        &output,
                        origin,
                        &mut last_pose,
                    )
                })();
                if let Err(e) = result {
                    final_error.get_or_insert(e);
                }
            }
            CaptureEvent::Done(result) => match result {
                Ok(v) => report["audio_capture"] = v,
                Err(e) => {
                    final_error.get_or_insert(e);
                }
            },
            CaptureEvent::Format(fmt) => ring.format(fmt),
        }
    }
    if let Err(e) = processor
        .finish()
        .and_then(|b| deliver(b, &mut ring, &output, origin, &mut last_pose))
    {
        final_error.get_or_insert(e);
    }
    while let Some(command) = commands.pop(Duration::ZERO) {
        if let Err(e) = control(command, &mut ring, &mut processor, &status, origin, false) {
            final_error.get_or_insert(e);
        }
    }
    report["status"] = json!(if final_error.is_some() {
        "failed"
    } else {
        "stopped"
    });
    if let Some(error) = final_error {
        report["error"] = json!(error.to_string());
    }
    ring.finish(if report["status"] == "failed" {
        "engine_error"
    } else {
        "input_disabled"
    });
    report["statistics"] = processor.statistics();
    report["ring"] = ring.stats();
    report["end_qpc_ns"] = json!(clock::now_ns());
    Ok(report)
}
