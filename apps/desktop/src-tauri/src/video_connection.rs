//! VRChat-only background Spout discovery. Expensive GPU receiver startup never
//! holds the shared desktop Studio mutex or runs in the audio callback.
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use vsm_video::{Preview, SenderInfo, VideoFrame, VideoSource};

pub const AUTO_SOURCE: &str = "__vsm_vrchat_auto";
pub const SYNTHETIC_SOURCE: &str = "__vsm_test_pattern";
#[derive(Clone, PartialEq)]
struct Config {
    enabled: bool,
    synthetic: bool,
    generation: u64,
}
struct State {
    config: Config,
    status: Value,
    frame: Option<VideoFrame>,
}

/// VRChat's sender format seen in actual recordings is VRCSender1, VRCSender2…
/// Generic active Spout senders must never become an automatic fallback.
fn is_vrchat(name: &str) -> bool {
    name.strip_prefix("VRCSender")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
}
fn select_sender(senders: &[SenderInfo], previous: &str) -> Option<SenderInfo> {
    let mut candidates: Vec<_> = senders.iter().filter(|s| is_vrchat(&s.name)).collect();
    candidates.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));
    candidates
        .iter()
        .find(|s| s.name == previous)
        .copied()
        .or_else(|| candidates.first().copied())
        .cloned()
}
pub struct AutoPreview {
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
    suspended: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl AutoPreview {
    pub fn new(enabled: bool, synthetic: bool) -> Result<Self> {
        let state = Arc::new(Mutex::new(State {
            config: Config {
                enabled,
                synthetic,
                generation: 0,
            },
            status: json!({"active":false,"state":if enabled{"waiting"}else{"disabled"},"sender_name":"","error":""}),
            frame: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let suspended = Arc::new(AtomicBool::new(false));
        let (s, c, p) = (state.clone(), cancel.clone(), suspended.clone());
        let worker = thread::Builder::new()
            .name("vsm-vrchat-discovery".into())
            .spawn(move || run(s, c, p))?;
        Ok(Self {
            state,
            cancel,
            suspended,
            worker: Some(worker),
        })
    }
    pub fn configure(&self, enabled: bool, synthetic: bool) {
        let mut state = self.state.lock().unwrap();
        state.config = Config {
            enabled,
            synthetic,
            generation: state.config.generation + 1,
        };
        state.frame = None;
        state.status = json!({"active":false,"state":if enabled{"waiting"}else{"disabled"},"sender_name":"","error":""});
    }
    pub fn suspend(&self, value: bool) {
        self.suspended.store(value, Ordering::Relaxed);
        if value {
            let mut state = self.state.lock().unwrap();
            // An in-flight publication from the previous receiver must not
            // restore a stale frame after recording has suspended preview.
            state.config.generation += 1;
            state.frame = None;
            state.status = json!({"active":false,"state":"recording","sender_name":"","error":""});
        }
    }
    pub fn status(&self) -> Value {
        self.state.lock().unwrap().status.clone()
    }
    pub fn frame(&self) -> Option<VideoFrame> {
        self.state.lock().unwrap().frame.clone()
    }
}
impl Drop for AutoPreview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn publish(state: &Mutex<State>, config: &Config, status: Value, frame: Option<VideoFrame>) {
    let mut state = state.lock().unwrap();
    if state.config == *config {
        state.status = status;
        state.frame = frame;
    }
}
fn run(state: Arc<Mutex<State>>, cancel: Arc<AtomicBool>, suspended: Arc<AtomicBool>) {
    let mut current: Option<Preview> = None;
    let mut current_name = String::new();
    let mut config = None;
    let mut retry = Instant::now();
    while !cancel.load(Ordering::Relaxed) {
        let next = state.lock().unwrap().config.clone();
        if config.as_ref() != Some(&next) {
            current.take();
            current_name.clear();
            config = Some(next.clone());
            retry = Instant::now();
        }
        if !next.enabled || suspended.load(Ordering::Relaxed) {
            current.take();
            current_name.clear();
            thread::sleep(Duration::from_millis(16));
            continue;
        }
        if let Some(preview) = &current {
            let status = preview.status();
            if status.active {
                publish(
                    &state,
                    &next,
                    json!({"active":true,"state":"connected","sender_name":current_name,"width":status.width,"height":status.height,"captured_frames":status.frames,"error":""}),
                    preview.latest_frame(),
                );
                thread::sleep(Duration::from_millis(16));
                continue;
            }
            publish(
                &state,
                &next,
                json!({"active":false,"state":"waiting","sender_name":"","error":status.error}),
                None,
            );
            current.take();
            retry = Instant::now();
        }
        if Instant::now() < retry {
            thread::sleep(Duration::from_millis(16));
            continue;
        }
        retry = Instant::now() + Duration::from_millis(500);
        let candidate = if next.synthetic {
            Some((
                SYNTHETIC_SOURCE.to_string(),
                VideoSource::Synthetic {
                    width: 1280,
                    height: 720,
                },
            ))
        } else {
            match vsm_video::list_senders() {
                Ok(senders) => select_sender(&senders, &current_name).map(|s| {
                    (
                        s.name.clone(),
                        VideoSource::Spout {
                            sender_name: s.name,
                        },
                    )
                }),
                Err(error) => {
                    publish(
                        &state,
                        &next,
                        json!({"active":false,"state":"waiting","sender_name":"","error":error.to_string()}),
                        None,
                    );
                    None
                }
            }
        };
        let Some((name, source)) = candidate else {
            publish(
                &state,
                &next,
                json!({"active":false,"state":"waiting","sender_name":"","error":"VRChatのSpout映像を待っています"}),
                None,
            );
            continue;
        };
        publish(
            &state,
            &next,
            json!({"active":false,"state":"connecting","sender_name":"","error":""}),
            None,
        );
        match Preview::open(source) {
            Ok(preview) => {
                if !cancel.load(Ordering::Relaxed)
                    && !suspended.load(Ordering::Relaxed)
                    && state.lock().unwrap().config == next
                {
                    current_name = name;
                    current = Some(preview);
                }
            }
            Err(error) => publish(
                &state,
                &next,
                json!({"active":false,"state":"waiting","sender_name":"","error":error.to_string()}),
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sender(name: &str, active: bool) -> SenderInfo {
        SenderInfo {
            name: name.into(),
            width: 3840,
            height: 2160,
            format: 87,
            active,
        }
    }
    #[test]
    fn vrchat_discovery_never_uses_unrelated_active_sender() {
        for name in [
            "OBS",
            "VRChat",
            "VRCSender",
            "VRCSender1-extra",
            "VRCSender１",
            "Spout Demo",
        ] {
            assert!(!is_vrchat(name), "{name}");
        }
        for name in ["VRCSender1", "VRCSender2", "VRCSender10"] {
            assert!(is_vrchat(name));
        }
        assert!(select_sender(&[sender("OBS", true)], "").is_none());
        assert_eq!(
            select_sender(&[sender("OBS", true), sender("VRCSender1", false)], "")
                .unwrap()
                .name,
            "VRCSender1"
        );
    }
    #[test]
    fn vrchat_discovery_keeps_existing_sender_before_active_switch() {
        let sources = [sender("VRCSender1", false), sender("VRCSender2", true)];
        assert_eq!(
            select_sender(&sources, "VRCSender1").unwrap().name,
            "VRCSender1"
        );
        assert_eq!(
            select_sender(&sources, "missing").unwrap().name,
            "VRCSender2"
        );
    }
    #[cfg(windows)]
    #[test]
    fn synthetic_auto_preview_reconnects_and_rejects_stale_generations() {
        fn frame(preview: &AutoPreview) -> VideoFrame {
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(frame) = preview.frame() {
                    return frame;
                }
                assert!(Instant::now() < until, "{:?}", preview.status());
                thread::sleep(Duration::from_millis(10));
            }
        }
        let preview = AutoPreview::new(false, true).unwrap();
        assert!(preview.frame().is_none());
        preview.configure(true, true);
        let first = frame(&preview);
        assert_eq!(preview.status()["sender_name"], SYNTHETIC_SOURCE);
        preview.configure(false, true);
        assert!(preview.frame().is_none());
        thread::sleep(Duration::from_millis(80));
        assert_eq!(preview.status()["state"], "disabled");
        assert!(preview.frame().is_none());
        preview.configure(true, true);
        assert!(frame(&preview).qpc_ns > first.qpc_ns);
        preview.suspend(true);
        thread::sleep(Duration::from_millis(80));
        assert!(preview.frame().is_none());
        assert_eq!(preview.status()["state"], "recording");
        preview.suspend(false);
        assert!(frame(&preview).qpc_ns > first.qpc_ns);
    }
}
