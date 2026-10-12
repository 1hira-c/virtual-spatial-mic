use crate::video_connection::{AUTO_SOURCE, AutoPreview, SYNTHETIC_SOURCE};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use vsm_core::wave;
use vsm_video::{AudioSink, Recorder, VideoConfig, VideoFrame, VideoSource};

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
    let audio_offset_ms = match value.get("audio_offset_ms") {
        None => 0,
        Some(offset) => offset
            .as_i64()
            .context("録画音声のタイミング調整は整数のミリ秒で指定してください")?,
    };
    ensure!(
        (-2000..=2000).contains(&audio_offset_ms),
        "録画音声のタイミング調整は-2000〜2000 msで指定してください"
    );
    ensure!(
        sender.len() <= 255 && !sender.contains('\0'),
        "映像入力の名前が不正です"
    );
    ensure!(fps == 30, "初版の録画は30 fpsです");
    // Preserve only the developer fixture. Product sources are always VRChat.
    let sender = if sender == SYNTHETIC_SOURCE {
        SYNTHETIC_SOURCE
    } else {
        AUTO_SOURCE
    };
    Ok(
        json!({"enabled":enabled,"sender_name":sender,"fps":fps,"automatic":sender!=SYNTHETIC_SOURCE,"audio_offset_ms":audio_offset_ms}),
    )
}

struct Active {
    start: i64,
    staging: PathBuf,
    directory: PathBuf,
    sender_name: String,
}

pub struct VideoRecording {
    config: Value,
    settings_path: PathBuf,
    recorder: Option<Recorder>,
    preview: AutoPreview,
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
        let preview = AutoPreview::new(
            config["enabled"] == true,
            config["sender_name"] == SYNTHETIC_SOURCE,
        )?;
        let out = Self {
            config,
            settings_path,
            recorder: None,
            preview,
            tap: Arc::new(Mutex::new(None)),
            active: None,
            last: json!({"active":false,"state":"idle","error":""}),
        };
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
        Ok(if self.config["sender_name"] == SYNTHETIC_SOURCE {
            VideoSource::Synthetic {
                width: 1280,
                height: 720,
            }
        } else {
            let connection = self.preview.status();
            ensure!(
                connection["active"] == true,
                "VRChatのカメラでSpoutをONにして、映像の接続を待ってください"
            );
            let name = connection["sender_name"]
                .as_str()
                .context("VRChat映像の接続先が不明です")?;
            VideoSource::Spout {
                sender_name: name.into(),
            }
        })
    }
    fn open_preview(&mut self) {
        self.preview.suspend(false);
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
        self.preview.configure(
            self.enabled(),
            self.config["sender_name"] == SYNTHETIC_SOURCE,
        );
        Ok(())
    }
    pub fn retry_preview(&mut self) {
        // Kept for the legacy enumeration command. Background discovery now
        // retries automatically, including Spout being enabled after startup.
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
        let sender_name = match &source {
            VideoSource::Spout { sender_name } => sender_name.clone(),
            VideoSource::Synthetic { .. } => SYNTHETIC_SOURCE.into(),
        };
        fs::create_dir_all(root)?;
        ensure!(
            fs2::available_space(root)? >= 1024 * 1024 * 1024,
            "保存開始には1 GiB以上の空き容量が必要です"
        );
        let requested = vsm_live::clock::now_ns();
        let staging = root.join(format!(".vsm-video-{}-{requested}.mp4", std::process::id()));
        self.preview.suspend(true);
        let recorder = match Recorder::start(
            &staging,
            VideoConfig {
                source,
                fps: 30,
                bitrate: 8_000_000,
                start_qpc_ns: 0,
                audio_offset_ns: self.config["audio_offset_ms"].as_i64().unwrap_or(0) * 1_000_000,
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
            sender_name,
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
            let audio_offset_ms = report.audio_offset_ns / 1_000_000;
            let mut metadata = serde_json::to_value(report)?;
            metadata["audio_offset_ms"] = json!(audio_offset_ms);
            metadata["path"] = json!("video.mp4");
            metadata["sender_name"] = json!(active.sender_name);
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
        let connection = self.preview.status();
        out["sender_name"] = self
            .active
            .as_ref()
            .map(|active| json!(active.sender_name))
            .unwrap_or_else(|| connection["sender_name"].clone());
        if self.config["sender_name"] == SYNTHETIC_SOURCE {
            out["sender_name"] = json!(SYNTHETIC_SOURCE);
        }
        out["connection"] = connection.clone();
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
            if self.enabled()
                && !["saved", "failed"].contains(&self.last["state"].as_str().unwrap_or(""))
                && connection["active"] != true
            {
                json!({"active":false,"state":"waiting","error":connection["error"].as_str().unwrap_or("")})
            } else {
                self.last.clone()
            }
        };
        out["preview"] = connection;
        out
    }
    /// Clone the latest immutable BGRA frame while briefly holding only its
    /// pointer lock. BMP resizing/encoding happens after the Studio lock drops.
    pub fn preview_frame(&self) -> Option<VideoFrame> {
        self.recorder
            .as_ref()
            .and_then(Recorder::latest_frame)
            .or_else(|| self.preview.frame())
    }
}

pub fn thumbnail(frame: &VideoFrame) -> Option<Value> {
    let image = bitmap(frame)?;
    Some(
        json!({"bmp_base64":STANDARD.encode(image.bytes),"width":image.width,"height":image.height}),
    )
}

pub struct Bitmap {
    pub bytes: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Binary BMP is also used by the custom preview protocol, avoiding base64 IPC.
pub fn bitmap(frame: &VideoFrame) -> Option<Bitmap> {
    let expected = (frame.width as usize)
        .checked_mul(frame.height as usize)?
        .checked_mul(4)?;
    if frame.width == 0 || frame.height == 0 || frame.bgra.len() != expected {
        return None;
    }
    const MAX_EDGE: u32 = 960;
    let edge = frame.width.max(frame.height);
    let dimension = |value: u32| {
        if edge <= MAX_EDGE {
            value as usize
        } else {
            // Round to the nearest pixel, retaining the aspect ratio in both
            // portrait and landscape. Native small frames are never enlarged.
            ((value as u64 * MAX_EDGE as u64 + edge as u64 / 2) / edge as u64).max(1) as usize
        }
    };
    let width = dimension(frame.width);
    let height = dimension(frame.height);
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
    Some(Bitmap {
        bytes,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn product_video_settings_are_always_vrchat_while_fixture_is_preserved() {
        for sender in ["", "OBS", "VRCSender1", "__vsm_vrchat_auto"] {
            let value = settings(&json!({"enabled":true,"sender_name":sender,"fps":30})).unwrap();
            assert_eq!(value["sender_name"], AUTO_SOURCE);
            assert_eq!(value["automatic"], true);
        }
        let value = settings(&json!({"sender_name":SYNTHETIC_SOURCE})).unwrap();
        assert_eq!(value["sender_name"], SYNTHETIC_SOURCE);
        assert_eq!(value["automatic"], false);
    }

    #[test]
    fn legacy_video_settings_default_audio_offset_to_zero() {
        for old in [
            json!({}),
            json!({"enabled":true,"sender_name":"VRCSender1","fps":30}),
            json!({"enabled":false,"sender_name":SYNTHETIC_SOURCE}),
        ] {
            assert_eq!(settings(&old).unwrap()["audio_offset_ms"], 0);
        }
    }

    #[test]
    fn audio_offset_accepts_integer_bounds_and_survives_source_normalization() {
        for offset in [-2000, -1, 0, 1, 2000] {
            for sender in ["VRCSender1", "OBS", AUTO_SOURCE, SYNTHETIC_SOURCE] {
                let normalized = settings(&json!({
                    "enabled":true,"sender_name":sender,"fps":30,"audio_offset_ms":offset,
                }))
                .unwrap();
                assert_eq!(normalized["audio_offset_ms"].as_i64(), Some(offset));
                assert_eq!(normalized["enabled"], true);
                assert_eq!(normalized["fps"], 30);
                assert_eq!(
                    normalized["sender_name"],
                    if sender == SYNTHETIC_SOURCE {
                        SYNTHETIC_SOURCE
                    } else {
                        AUTO_SOURCE
                    }
                );
                assert_eq!(settings(&normalized).unwrap(), normalized);
            }
        }
    }

    #[test]
    fn audio_offset_rejects_invalid_types_fractional_numbers_and_range() {
        for invalid in [
            json!(-2001),
            json!(2001),
            json!(i64::MIN),
            json!(i64::MAX),
            json!(u64::MAX),
            json!(-0.5),
            json!(0.5),
            json!(1.0),
            json!("100"),
            json!(null),
            json!(true),
            json!([]),
            json!({}),
        ] {
            assert!(settings(&json!({"audio_offset_ms":invalid})).is_err());
        }
    }
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

    fn solid_frame(width: u32, height: u32) -> VideoFrame {
        VideoFrame {
            width,
            height,
            qpc_ns: 0,
            bgra: Arc::new([23, 47, 91, 255].repeat(width as usize * height as usize)),
        }
    }

    #[test]
    fn thumbnail_caps_long_edge_without_upscaling_or_distorting_orientation() {
        for (width, height, expected_width, expected_height) in [
            (1920, 1080, 960, 540),
            (1080, 1920, 540, 960),
            (3840, 2160, 960, 540),
            (640, 360, 640, 360),
            (360, 640, 360, 640),
            (960, 960, 960, 960),
            (1001, 563, 960, 540),
        ] {
            let frame = solid_frame(width, height);
            let shared = frame.bgra.clone();
            let image = thumbnail(&frame).unwrap();
            assert_eq!(image["width"], expected_width);
            assert_eq!(image["height"], expected_height);
            assert!(Arc::ptr_eq(&frame.bgra, &shared));
            let bytes = STANDARD
                .decode(image["bmp_base64"].as_str().unwrap())
                .unwrap();
            let stride = (expected_width as usize * 3 + 3) & !3;
            assert_eq!(bytes.len(), 54 + stride * expected_height as usize);
            assert_eq!(
                u32::from_le_bytes(bytes[2..6].try_into().unwrap()) as usize,
                bytes.len()
            );
            assert_eq!(
                i32::from_le_bytes(bytes[18..22].try_into().unwrap()),
                expected_width
            );
            assert_eq!(
                i32::from_le_bytes(bytes[22..26].try_into().unwrap()),
                -expected_height
            );
            assert_eq!(&bytes[54..57], &[23, 47, 91]);
            let last =
                54 + (expected_height as usize - 1) * stride + (expected_width as usize - 1) * 3;
            assert_eq!(&bytes[last..last + 3], &[23, 47, 91]);
        }
    }

    #[test]
    fn thumbnail_samples_scaled_coordinates_and_rejects_invalid_frames() {
        let mut frame = solid_frame(1920, 2);
        let pixels = Arc::make_mut(&mut frame.bgra);
        pixels[0..4].copy_from_slice(&[1, 2, 3, 255]);
        pixels[1918 * 4..1918 * 4 + 4].copy_from_slice(&[4, 5, 6, 255]);
        let image = thumbnail(&frame).unwrap();
        assert_eq!(image["width"], 960);
        assert_eq!(image["height"], 1);
        let bytes = STANDARD
            .decode(image["bmp_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(&bytes[54..57], &[1, 2, 3]);
        assert_eq!(&bytes[54 + 959 * 3..54 + 960 * 3], &[4, 5, 6]);
        for (width, height, pixels) in [
            (0, 3, vec![]),
            (2, 2, vec![0; 15]),
            (u32::MAX, u32::MAX, vec![]),
        ] {
            assert!(
                thumbnail(&VideoFrame {
                    width,
                    height,
                    qpc_ns: 0,
                    bgra: Arc::new(pixels)
                })
                .is_none()
            );
        }
    }

    #[test]
    #[ignore = "Developer performance diagnostic; timings are not a correctness threshold"]
    fn thumbnail_960_preview_cost() {
        let frame = solid_frame(1920, 1080);
        let before = std::time::Instant::now();
        let mut bytes = 0;
        for _ in 0..80 {
            let image = std::hint::black_box(thumbnail(std::hint::black_box(&frame)).unwrap());
            bytes = image["bmp_base64"].as_str().unwrap().len();
        }
        let elapsed = before.elapsed();
        eprintln!(
            "960×540 thumbnail+base64: {:?}/frame, {} encoded bytes/frame, {} encoded bytes/second at 4 fps",
            elapsed / 80,
            bytes,
            bytes * 4
        );
    }
}
