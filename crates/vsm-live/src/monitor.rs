use anyhow::{Result, ensure};
use serde_json::{Value, json};
#[cfg(windows)]
use std::time::{Duration, Instant};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
struct Block {
    samples: Vec<f32>,
    #[cfg_attr(not(windows), allow(dead_code))]
    at: i64,
    cursor: usize,
}
struct State {
    blocks: VecDeque<Block>,
    queued: usize,
    delivered: u64,
    dropped: u64,
    ready: bool,
    error: String,
}
pub struct Monitor {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Monitor {
    pub fn new(endpoint: String) -> Result<Self> {
        let state = Arc::new(Mutex::new(State {
            blocks: VecDeque::new(),
            queued: 0,
            delivered: 0,
            dropped: 0,
            ready: false,
            error: String::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let data = state.clone();
        let cancel = stop.clone();
        let worker = thread::Builder::new()
            .name("vsm-monitor".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    #[cfg(windows)]
                    {
                        let output = crate::wasapi::Output::open(&endpoint)?;
                        let mut last = Instant::now();
                        while !cancel.load(Ordering::Relaxed) {
                            let Some((count, now)) = output.wait()? else {
                                ensure!(
                                    last.elapsed() < Duration::from_secs(3),
                                    "音声出力からの応答が3秒停止しました"
                                );
                                continue;
                            };
                            last = Instant::now();
                            let mut samples = vec![0.; count as usize * 2];
                            {
                                let mut state = data.lock().unwrap();
                                state.ready = true;
                                for i in 0..count as usize {
                                    let at = now + i as i64 * 1_000_000_000 / 48000;
                                    while state.blocks.front().is_some_and(|b| {
                                        b.at + (b.samples.len() / 2) as i64 * 1_000_000_000 / 48000
                                            < at - 100_000_000
                                    }) {
                                        let b = state.blocks.pop_front().unwrap();
                                        state.queued -= b.samples.len() / 2 - b.cursor;
                                        state.dropped += 1;
                                    }
                                    let Some(block) = state.blocks.front_mut() else {
                                        break;
                                    };
                                    if block.at + block.cursor as i64 * 1_000_000_000 / 48000 > at {
                                        continue;
                                    }
                                    samples[i * 2] = block.samples[block.cursor * 2];
                                    samples[i * 2 + 1] = block.samples[block.cursor * 2 + 1];
                                    block.cursor += 1;
                                    let end = block.cursor * 2 == block.samples.len();
                                    state.queued -= 1;
                                    state.delivered += 1;
                                    if end {
                                        state.blocks.pop_front();
                                    }
                                }
                            }
                            output.write(&samples)?;
                        }
                        Ok(())
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = (&endpoint, &cancel);
                        anyhow::bail!("このOSのライブ出力は未実装です")
                    }
                })();
                if let Err(e) = result {
                    let mut state = data.lock().unwrap();
                    state.error = e.to_string();
                    state.ready = false;
                }
            })?;
        Ok(Self {
            state,
            stop,
            worker: Some(worker),
        })
    }
    pub fn push(&self, samples: &[f32], at: i64) -> Result<()> {
        ensure!(
            samples.len() % 2 == 0 && samples.len() / 2 <= 48000,
            "Invalid monitor block"
        );
        let mut state = self.state.lock().unwrap();
        while state.queued + samples.len() / 2 > 48000 {
            if let Some(block) = state.blocks.pop_front() {
                state.queued -= block.samples.len() / 2 - block.cursor;
                state.dropped += 1;
            } else {
                break;
            }
        }
        state.queued += samples.len() / 2;
        state.blocks.push_back(Block {
            samples: samples.to_vec(),
            at,
            cursor: 0,
        });
        Ok(())
    }
    pub fn status(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"ready":s.ready,"error":s.error,"delivered_frames":s.delivered,"dropped_blocks":s.dropped,"queued_frames":s.queued})
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
