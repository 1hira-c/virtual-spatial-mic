#[cfg(windows)]
#[path = "support/spout_sender.rs"]
mod spout_sender;
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::{path::Path, thread, time::Duration};
    use vsm_video::{Recorder, VideoConfig, VideoSource};
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("senders") => {
            for sender in vsm_video::list_senders()? {
                println!(
                    "{} {}×{} format={} active={}",
                    sender.name, sender.width, sender.height, sender.format, sender.active
                );
            }
        }
        Some("inspect") => {
            let path = Path::new(args.get(2).ok_or_else(|| anyhow::anyhow!("Missing path"))?);
            let (width, height, rate, channels) = vsm_video::inspect(path)?;
            println!(
                "H.264 {width}×{height}; AAC {rate} Hz {channels} channels; bytes={}",
                std::fs::metadata(path)?.len()
            );
        }
        Some("decode-check") => {
            let path = Path::new(args.get(2).ok_or_else(|| anyhow::anyhow!("Missing path"))?);
            let report = vsm_video::decode_check(path)?;
            println!("{report:?}");
        }
        Some("spout-sender") => {
            let seconds: u64 = args.get(2).map_or(Ok(30), |s| s.parse())?;
            let sender = spout_sender::RunningSender::start()?;
            println!("sender={} duration_seconds={seconds}", sender.name);
            thread::sleep(Duration::from_secs(seconds));
        }
        Some(
            mode @ ("record-synthetic" | "record-auto" | "record-spout-test" | "record-spout-drop"),
        ) => {
            let path = Path::new(args.get(2).ok_or_else(|| anyhow::anyhow!("Missing path"))?);
            let seconds: f64 = args.get(3).map_or(Ok(5.), |s| s.parse())?;
            anyhow::ensure!(
                (1.0..=60.0).contains(&seconds),
                "Duration must be 1–60 seconds"
            );
            let sender = if mode.starts_with("record-spout-") {
                Some(spout_sender::RunningSender::start()?)
            } else {
                None
            };
            let source = if let Some(sender) = &sender {
                VideoSource::Spout {
                    sender_name: sender.name.clone(),
                }
            } else {
                VideoSource::Synthetic {
                    width: 640,
                    height: 360,
                }
            };
            let requested_origin = if mode == "record-auto" {
                0
            } else {
                vsm_video::now_ns()
            };
            let mut recorder = Recorder::start(
                path,
                VideoConfig {
                    source,
                    fps: 30,
                    bitrate: 3_000_000,
                    start_qpc_ns: requested_origin,
                },
            )?;
            let origin = recorder.status().origin_qpc_ns;
            let sender = if mode == "record-spout-drop" {
                let sender = sender.unwrap();
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(2));
                    drop(sender);
                });
                None
            } else {
                sender
            };
            let sink = recorder.audio_sink();
            let limit = (seconds * 48000.) as usize;
            let producer = thread::spawn(move || -> anyhow::Result<()> {
                let mut first = 0;
                while first < limit {
                    let count = 256.min(limit - first);
                    let mut samples = Vec::with_capacity(count * 2);
                    for frame in first..first + count {
                        let t = frame as f64 / 48000.;
                        samples.push((t * 440. * std::f64::consts::TAU).sin() as f32 * 0.12);
                        samples.push((t * 660. * std::f64::consts::TAU).sin() as f32 * 0.12);
                    }
                    let source_ns = origin + first as i64 * 1_000_000_000 / 48000;
                    // Simulate the engine's 100 ms processing delay and preserve source time.
                    while vsm_video::now_ns() < source_ns + 100_000_000 {
                        thread::sleep(Duration::from_millis(1));
                    }
                    sink.push(&samples, source_ns)?;
                    first += count;
                }
                Ok(())
            });
            let report = recorder.stop_at(origin + (seconds * 1e9) as i64)?;
            producer
                .join()
                .map_err(|_| anyhow::anyhow!("Audio fixture producer panicked"))??;
            if mode == "record-spout-drop" {
                anyhow::ensure!(
                    report.warning.is_some(),
                    "Sender loss did not produce a saved warning report"
                );
                anyhow::ensure!(
                    report.video_frames <= (seconds * 30.).ceil() as u64,
                    "Sender-loss recording exceeded cutoff"
                );
            } else {
                anyhow::ensure!(
                    report.video_frames == (seconds * 30.).ceil() as u64,
                    "Video cutoff differs from the requested duration"
                );
                anyhow::ensure!(
                    report.audio_frames == limit as u64,
                    "Delayed audio tail was not fully saved"
                );
            }
            println!(
                "saved={} frames={} audio_frames={} repeat={} dimensions={}×{}",
                report.path,
                report.video_frames,
                report.audio_frames,
                report.repeated_frames,
                report.width,
                report.height
            );
            let info = vsm_video::inspect(path)?;
            println!("inspect={info:?}");
            println!("warning={:?}", report.warning);
            drop(sender);
        }
        _ => anyhow::bail!(
            "Usage: video-check senders | inspect <mp4> | decode-check <mp4> | record-synthetic <mp4> [seconds] | record-auto <mp4> [seconds] | record-spout-test <mp4> [seconds] | record-spout-drop <mp4> [seconds] | spout-sender [seconds]"
        ),
    }
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("video-check requires Windows");
}
