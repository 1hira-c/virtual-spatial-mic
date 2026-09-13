use crate::clock;
#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};
use vsm_core::{processor::AudioPacket, wave::WaveReader};

#[derive(Clone, Debug)]
pub struct CapturedPacket {
    pub audio: AudioPacket,
    pub qpc_100ns: u64,
    pub receive_ticks: i64,
}
pub fn capture(
    config: &Value,
    origin: i64,
    cancel: &AtomicBool,
    mut format: impl FnMut(Value) -> Result<()>,
    mut accept: impl FnMut(CapturedPacket) -> Result<()>,
) -> Result<Value> {
    let seconds = config.get("seconds").and_then(Value::as_f64).unwrap_or(0.);
    ensure!(
        seconds.is_finite() && (0.0..=86400.0).contains(&seconds),
        "Invalid capture duration"
    );
    if config["fixture"] == true {
        format(
            json!({"sample_rate_hz":48000,"channels":2,"container_bits":32,"valid_bits":32,"channel_mask":3,"file_sample_format":"float32","hardware_path_format":"synthetic_fixture"}),
        )?;
        let mut fixture = config["fixture_audio_path"]
            .as_str()
            .map(|p| WaveReader::open(Path::new(p)))
            .transpose()?;
        if let Some(w) = &fixture {
            ensure!(w.channels == 2, "Fixture requires stereo");
        }
        let limit = if seconds == 0. {
            i64::MAX as u64 / 8
        } else {
            (seconds * 48000.) as u64
        }
        .min(fixture.as_ref().map_or(u64::MAX, |w| w.frames));
        let offset = config["fixture_audio_offset_ns"].as_i64().unwrap_or(0);
        let mut first = 0;
        while first < limit && !cancel.load(Ordering::Relaxed) {
            let count = 480u64.min(limit - first) as u32;
            let due = origin + offset + ((first + count as u64) * 1_000_000_000 / 48000) as i64;
            while clock::now_ns() < due && !cancel.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(1));
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            ensure!(
                !config["fixture_fail_after_frames"]
                    .as_u64()
                    .is_some_and(|n| first >= n),
                "Synthetic fixture injected capture failure"
            );
            let mut flags = 0;
            let mut device = first;
            let mut qpc = ((origin + offset) / 100) as u64 + first * 10_000_000 / 48000;
            if config["fixture_faults"] == true {
                if first == 480 {
                    flags = 2;
                }
                if first >= 960 {
                    device += 480;
                }
                if first == 960 {
                    flags = 1;
                }
                if first == 1440 {
                    flags = 4;
                    qpc = 0;
                }
            }
            let samples = if let Some(w) = &mut fixture {
                w.read(first, count)?
            } else if flags & 2 != 0 {
                vec![0.; count as usize * 2]
            } else {
                (0..count)
                    .flat_map(|i| {
                        [
                            (0.1 * ((first + i as u64) as f64 * 0.03).sin()) as f32,
                            0.025,
                        ]
                    })
                    .collect()
            };
            accept(CapturedPacket {
                audio: AudioPacket {
                    samples,
                    frames: count,
                    channels: 2,
                    flags,
                    device,
                    file: first,
                    time: qpc as i64 * 100 - origin,
                },
                qpc_100ns: qpc,
                receive_ticks: clock::ticks(),
            })?;
            first += count as u64;
        }
        return Ok(
            json!({"status":"stopped","backend":"synthetic_fixture","acquired_frames":first,"stop_reason":if first>=limit{"fixture_eof"}else{"cancelled"}}),
        );
    }
    #[cfg(windows)]
    {
        crate::wasapi::capture(
            config["endpoint_id"]
                .as_str()
                .context("マイクを選んでください")?,
            origin,
            seconds,
            cancel,
            format,
            accept,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (format, accept, origin, cancel);
        anyhow::bail!("このOSの録音バックエンドは未実装です")
    }
}
