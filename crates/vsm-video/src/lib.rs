//! Windows-native Spout reception and H.264/AAC MP4 recording.
//!
//! Input audio uses the capture/source QPC timestamp, not its delayed monitor
//! presentation timestamp. Video and audio are then written on the same clock.

use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
    thread::JoinHandle,
};

#[cfg(windows)]
mod platform;
#[cfg(windows)]
pub use platform::{DecodeReport, decode_check, inspect};
mod pixels;
#[cfg(windows)]
pub mod spout;

#[derive(Clone, Debug)]
pub enum VideoSource {
    Spout { sender_name: String },
    Synthetic { width: u32, height: u32 },
}

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub source: VideoSource,
    pub fps: u32,
    pub bitrate: u32,
    /// Zero chooses the QPC clock immediately after encoder initialization.
    /// A positive clock is useful for deterministic developer fixtures.
    pub start_qpc_ns: i64,
}
impl VideoConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=60).contains(&self.fps),
            "Video frame rate must be 1–60 fps"
        );
        ensure!(
            (100_000..=80_000_000).contains(&self.bitrate),
            "Video bitrate outside supported bounds"
        );
        if let VideoSource::Synthetic { width, height } = self.source {
            ensure!(
                (16..=4096).contains(&width)
                    && (16..=4096).contains(&height)
                    && width % 2 == 0
                    && height % 2 == 0,
                "Video dimensions must be even and 16–4096"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VideoStatus {
    pub active: bool,
    pub origin_qpc_ns: i64,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub audio_frames: u64,
    pub repeated_frames: u64,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct VideoReport {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_frames: u64,
    pub audio_frames: u64,
    pub repeated_frames: u64,
    pub origin_qpc_ns: i64,
    pub end_qpc_ns: i64,
    pub video_codec: &'static str,
    pub audio_codec: &'static str,
    pub warning: Option<String>,
}

enum Command {
    Audio(Vec<f32>, i64),
    Stop(i64),
}

#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub qpc_ns: i64,
    /// Top-down BGRA, tightly packed, alpha is always 255.
    pub bgra: Arc<Vec<u8>>,
}
type Latest = Arc<Mutex<Option<VideoFrame>>>;

#[derive(Clone)]
pub struct AudioSink {
    tx: SyncSender<Command>,
    status: Arc<Mutex<VideoStatus>>,
}
impl AudioSink {
    /// Stereo interleaved 48 kHz float audio and QPC capture timestamp.
    /// This call never blocks the real-time audio engine.
    pub fn push(&self, samples: &[f32], source_qpc_ns: i64) -> Result<()> {
        ensure!(
            samples.len() % 2 == 0 && samples.len() <= 96_000,
            "Invalid stereo audio packet"
        );
        if !self.status.lock().unwrap().active {
            return Ok(());
        }
        match self
            .tx
            .try_send(Command::Audio(samples.to_vec(), source_qpc_ns))
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.status.lock().unwrap().error =
                    Some("動画の音声キューが上限に達しました".into());
                // Video failure must not stop the shared audio engine.
                Ok(())
            }
        }
    }
}

pub struct Recorder {
    sink: AudioSink,
    latest: Latest,
    worker: Option<JoinHandle<Result<VideoReport>>>,
}
impl Recorder {
    #[cfg(windows)]
    pub fn start(path: &Path, config: VideoConfig) -> Result<Self> {
        config.validate()?;
        ensure!(!path.exists(), "Video output already exists");
        platform::start(path, config)
    }
    #[cfg(not(windows))]
    pub fn start(_path: &Path, _config: VideoConfig) -> Result<Self> {
        anyhow::bail!("Standalone video recording currently requires Windows")
    }
    pub fn audio_sink(&self) -> AudioSink {
        self.sink.clone()
    }
    pub fn status(&self) -> VideoStatus {
        self.sink.status.lock().unwrap().clone()
    }
    pub fn latest_frame(&self) -> Option<VideoFrame> {
        self.latest.lock().unwrap().clone()
    }
    pub fn stop(&mut self) -> Result<VideoReport> {
        self.stop_at(now_ns())
    }
    /// Keep the video cutoff fixed and drain delayed processed audio for 350 ms.
    pub fn stop_at(&mut self, cutoff_qpc_ns: i64) -> Result<VideoReport> {
        let worker = self
            .worker
            .take()
            .ok_or_else(|| anyhow::anyhow!("Recorder already stopped"))?;
        let _ = self.sink.tx.send(Command::Stop(cutoff_qpc_ns));
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("Video recording worker panicked"))?
    }
}

pub struct Preview {
    latest: Latest,
    status: Arc<Mutex<VideoStatus>>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Preview {
    #[cfg(windows)]
    pub fn open(source: VideoSource) -> Result<Self> {
        platform::preview(source)
    }
    #[cfg(not(windows))]
    pub fn open(_source: VideoSource) -> Result<Self> {
        anyhow::bail!("Video preview currently requires Windows")
    }
    pub fn latest_frame(&self) -> Option<VideoFrame> {
        self.latest.lock().unwrap().clone()
    }
    pub fn status(&self) -> VideoStatus {
        self.status.lock().unwrap().clone()
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
pub use spout::{SenderInfo, list_senders};
#[cfg(not(windows))]
#[derive(Clone, Debug, Serialize)]
pub struct SenderInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub active: bool,
}
#[cfg(not(windows))]
pub fn list_senders() -> Result<Vec<SenderInfo>> {
    Ok(Vec::new())
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.stop();
        }
    }
}

#[cfg(windows)]
pub fn now_ns() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let frequency = *FREQ.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f).expect("QPC frequency") };
        f
    });
    let mut ticks = 0;
    unsafe { QueryPerformanceCounter(&mut ticks).expect("QPC counter") };
    (ticks as i128 * 1_000_000_000 / frequency as i128) as i64
}
#[cfg(not(windows))]
pub fn now_ns() -> i64 {
    0
}
