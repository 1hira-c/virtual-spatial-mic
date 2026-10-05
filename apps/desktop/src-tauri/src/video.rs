use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use vsm_core::wave;
use vsm_video::{AudioSink, Preview, Recorder, VideoConfig, VideoFrame, VideoSource};

const DELIVERY_DELAY_NS: i64 = 300_000_000;
pub type VideoOutput = Arc<dyn Fn(&[f32], i64) + Send + Sync>;

fn settings(value: &Value) -> Result<Value> {
    ensure!(value.is_object(), "Invalid video settings");
    let enabled = value
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sender = value
        .get("sender_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fps = value.get("fps").and_then(Value::as_u64).unwrap_or(30);
    ensure!(
        sender.len() <= 255 && !sender.contains('\0'),
        "映像入力の名前が不正です"
    );
    ensure!(fps == 30, "初版の録画は30 fpsです");
    Ok(json!({"enabled":enabled,"sender_name":sender,"fps":fps}))
}

struct Active {
    start: i64,
    staging: PathBuf,
    directory: PathBuf,
}

pub struct VideoRecording {
    config: Value,
    settings_path: PathBuf,
    recorder: Option<Recorder>,
    preview: Option<Preview>,
    tap: Arc<Mutex<Option<AudioSink>>>,
    active: Option<Active>,
    last: Value,
}

impl VideoRecording {
    pub fn load(settings_path: PathBuf) -> Result<Self> {
        let config = settings(&if settings_path.exists() {
            wave::load_json(&settings_path)?
        } else {
            json!({})
        })?;
        let mut out = Self {
            config,
            settings_path,
            recorder: None,
            preview: None,
            tap: Arc::new(Mutex::new(None)),
            active: None,
            last: json!({"active":false,"state":"idle","error":""}),
        };
        out.open_preview();
        Ok(out)
    }
    pub fn enabled(&self) -> bool {
        self.config["enabled"] == true
    }
    pub fn recording(&self) -> bool {
        self.recorder.is_some()
    }
    pub fn failed(&self) -> bool {
        self.recorder.as_ref().is_some_and(|r| {
            let s = r.status();
            !s.active || s.error.is_some()
        })
    }
    fn source(&self) -> Result<VideoSource> {
        let name = self.config["sender_name"].as_str().unwrap();
        ensure!(!name.is_empty(), "映像入力を選んでください");
        Ok(if name == "__vsm_test_pattern" {
            VideoSource::Synthetic {
                width: 1280,
                height: 720,
            }
        } else {
            VideoSource::Spout {
                sender_name: name.into(),
            }
        })
    }
    fn open_preview(&mut self) {
        self.preview.take();
        if !self.enabled() || self.config["sender_name"] == "" {
            return;
        }
        match self.source().and_then(Preview::open) {
            Ok(preview) => self.preview = Some(preview),
            Err(error) => {
                if self.last["state"] != "saved" && self.last["state"] != "failed" {
                    self.last =
                        json!({"active":false,"state":"waiting","error":format!("{error:#}")});
                }
            }
        }
    }
    pub fn configure(&mut self, value: &Value) -> Result<()> {
        ensure!(
            !self.recording(),
            "録画を停止してから映像設定を変更してください"
        );
        let normalized = settings(value)?;
        let parent = self
            .settings_path
            .parent()
            .context("映像設定の保存先がありません")?;
        fs::create_dir_all(parent)?;
        wave::save_json(&self.settings_path, &normalized)?;
        self.config = normalized;
        self.last = json!({"active":false,"state":"idle","error":""});
        self.open_preview();
        Ok(())
    }
    pub fn retry_preview(&mut self) {
        if !self.recording() && self.preview.as_ref().is_none_or(|p| !p.status().active) {
            self.open_preview();
        }
    }
    pub fn recording_name(&self) -> String {
        self.active
            .as_ref()
            .map(|a| {
                a.directory
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default()
    }
    pub fn audio_output(&self) -> VideoOutput {
        let tap = self.tap.clone();
        Arc::new(move |samples, scheduled| {
            let sink = tap.lock().unwrap().clone();
            if let Some(sink) = sink {
                // The MP4 aligns capture times. Monitor scheduling stays delayed.
                // A video failure must not interrupt audio capture or its archive.
                let _ = sink.push(samples, scheduled - DELIVERY_DELAY_NS);
            }
        })
    }
    pub fn begin(&mut self, root: &Path) -> Result<i64> {
        ensure!(!self.recording(), "録画中です");
        let source = self.source()?;
        fs::create_dir_all(root)?;
        ensure!(
            fs2::available_space(root)? >= 1024 * 1024 * 1024,
            "保存開始には1 GiB以上の空き容量が必要です"
        );
        let requested = vsm_live::clock::now_ns();
        let staging = root.join(format!(".vsm-video-{}-{requested}.mp4", std::process::id()));
        self.preview.take();
        let recorder = match Recorder::start(
            &staging,
            VideoConfig {
                source,
                fps: 30,
                bitrate: 8_000_000,
                start_qpc_ns: 0,
            },
        ) {
            Ok(recorder) => recorder,
            Err(error) => {
                self.last = json!({"active":false,"state":"failed","error":format!("{error:#}")});
                self.open_preview();
                return Err(error);
            }
        };
        let at = recorder.status().origin_qpc_ns;
        let started = chrono::Local::now()
            - chrono::Duration::nanoseconds(vsm_live::clock::now_ns().saturating_sub(at));
        let directory = root.join(started.format("%Y%m%d-%H%M%S").to_string());
        if directory.exists() {
            drop(recorder);
            self.open_preview();
            anyhow::bail!("同じ開始日時の収録が存在します。1秒以上あけて開始してください");
        }
        *self.tap.lock().unwrap() = Some(recorder.audio_sink());
        self.active = Some(Active {
            start: at,
            staging,
            directory,
        });
        self.recorder = Some(recorder);
        Ok(at)
    }
    pub fn finish(&mut self, cutoff: i64) -> Result<()> {
        let Some(mut recorder) = self.recorder.take() else {
            return Ok(());
        };
        let active = self.active.take().context("録画保存先が不明です")?;
        self.last = json!({"active":false,"state":"finalizing","error":""});
        // The audio engine is still running while this drains the delayed tail.
        let stopped = recorder.stop_at(cutoff);
        *self.tap.lock().unwrap() = None;
        let result = (|| -> Result<Value> {
            let report = stopped?;
            let manifest = wave::load_json(&active.directory.join("session.json"))
                .context("音声の保存先を作成できませんでした。動画は一時ファイルに残っています")?;
            ensure!(
                manifest["recording_start_qpc_ns"].as_i64() == Some(active.start),
                "音声と動画の収録開始が一致しません"
            );
            let destination = active.directory.join("video.mp4");
            ensure!(!destination.exists(), "動画の保存先が既に存在します");
            fs::rename(&active.staging, &destination)?;
            let mut metadata = serde_json::to_value(report)?;
            metadata["path"] = json!("video.mp4");
            metadata["sender_name"] = self.config["sender_name"].clone();
            metadata["timeline"] = json!("source_capture_qpc");
            metadata["monitor_delivery_delay_removed_ns"] = json!(DELIVERY_DELAY_NS);
            metadata["preroll_included"] = json!(false);
            metadata["audio"] = json!("VSM processed microphone only");
            metadata["status"] = json!("saved");
            wave::save_json(&active.directory.join("video.json"), &metadata)?;
            Ok(
                json!({"active":false,"state":"saved","error":metadata["warning"].as_str().unwrap_or(""),"path":destination,"frames":metadata["video_frames"],"report":metadata}),
            )
        })();
        self.last = match &result {
            Ok(report) => report.clone(),
            Err(error) => {
                json!({"active":false,"state":"failed","error":format!("{error:#}"),"path":active.staging})
            }
        };
        self.open_preview();
        result.map(|_| ())
    }
    pub fn status(&self) -> Value {
        let mut out = self.config.clone();
        out["status"] = if let Some(recorder) = &self.recorder {
            let mut status = serde_json::to_value(recorder.status()).unwrap_or_default();
            status["state"] = json!(if status["active"] == true {
                "recording"
            } else {
                "failed"
            });
            status["path"] = self
                .active
                .as_ref()
                .map(|a| json!(a.directory.join("video.mp4")))
                .unwrap_or(Value::Null);
            status
        } else {
            self.last.clone()
        };
        if let Some(preview) = &self.preview {
            out["preview"] = serde_json::to_value(preview.status()).unwrap_or_default();
        }
        out
    }
    pub fn preview_image(&self) -> Value {
        let frame = self
            .recorder
            .as_ref()
            .and_then(Recorder::latest_frame)
            .or_else(|| {
                self.preview
                    .as_ref()
                    .filter(|p| p.status().active)
                    .and_then(Preview::latest_frame)
            });
        frame.as_ref().and_then(thumbnail).unwrap_or(Value::Null)
    }
}

fn thumbnail(frame: &VideoFrame) -> Option<Value> {
    if frame.width == 0
        || frame.height == 0
        || frame.bgra.len() != frame.width as usize * frame.height as usize * 4
    {
        return None;
    }
    let width = frame.width.min(320) as usize;
    let height = (frame.height as usize * width / frame.width as usize).max(1);
    let stride = (width * 3 + 3) & !3;
    let length = 54 + stride * height;
    let mut bytes = vec![0u8; length];
    bytes[..2].copy_from_slice(b"BM");
    bytes[2..6].copy_from_slice(&(length as u32).to_le_bytes());
    bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bytes[22..26].copy_from_slice(&(-(height as i32)).to_le_bytes());
    bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
    bytes[34..38].copy_from_slice(&((stride * height) as u32).to_le_bytes());
    for y in 0..height {
        for x in 0..width {
            let index = ((y * frame.height as usize / height) * frame.width as usize
                + x * frame.width as usize / width)
                * 4;
            let target = 54 + y * stride + x * 3;
            bytes[target..target + 3].copy_from_slice(&frame.bgra[index..index + 3]);
        }
    }
    Some(json!({"bmp_base64":STANDARD.encode(bytes),"width":width,"height":height}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thumbnail_preserves_top_down_channels_and_scanline_padding() {
        let frame = VideoFrame {
            width: 2,
            height: 2,
            qpc_ns: 0,
            bgra: Arc::new(vec![
                1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255,
            ]),
        };
        let image = thumbnail(&frame).unwrap();
        let bytes = STANDARD
            .decode(image["bmp_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(&bytes[..2], b"BM");
        assert_eq!(&bytes[54..62], &[1, 2, 3, 4, 5, 6, 0, 0]);
        assert_eq!(&bytes[62..70], &[7, 8, 9, 10, 11, 12, 0, 0]);
        assert_eq!(i32::from_le_bytes(bytes[22..26].try_into().unwrap()), -2);
    }
}
