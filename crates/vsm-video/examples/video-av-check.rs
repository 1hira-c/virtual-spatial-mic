//! Developer-only AV clock fixture. Copy beside video-check.rs to run; no new dependencies.
#[cfg(windows)]
#[path = "support/spout_av_sender.rs"]
mod spout_av_sender;

const RATE: usize = 48_000;
const SECONDS: usize = 8;
const BLOCK: usize = 256;
const PULSE_FRAMES: usize = RATE * 30 / 1000;
const AUDIO_DELIVERY_DELAY_NS: i64 = 100_000_000;

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn pulse_sample(frame: usize) -> f32 {
    let second = frame / RATE;
    let phase = frame % RATE;
    if (1..=6).contains(&second) && phase < PULSE_FRAMES {
        // Cosine gives a nonzero exact first sample; matched analysis handles AAC pre-echo.
        (std::f64::consts::TAU * 1000.0 * phase as f64 / RATE as f64).cos() as f32 * 0.4
    } else {
        0.0
    }
}

#[cfg(windows)]
fn run(output: &std::path::Path, hold_ms: u64, audio_offset_ns: i64) -> anyhow::Result<String> {
    use std::{fs, thread, time::Duration};
    use vsm_video::{Recorder, VideoConfig, VideoSource};
    let scenario = if hold_ms == 0 {
        "normal".to_owned()
    } else {
        format!("mutex{hold_ms}")
    };
    let scenario = if audio_offset_ns == 0 {
        scenario
    } else {
        format!("{scenario}-offset{:+}ms", audio_offset_ns / 1_000_000)
    };
    let video_path = output.join(format!("av-{scenario}.mp4"));
    let json_path = output.join(format!("av-{scenario}.json"));
    anyhow::ensure!(
        !video_path.exists() && !json_path.exists(),
        "Fixture output already exists"
    );
    let sender = spout_av_sender::RunningSender::start(hold_ms)?;
    // Initialize the encoder first. Both stimulus timelines then use its actual origin.
    let mut recorder = Recorder::start(
        &video_path,
        VideoConfig {
            source: VideoSource::Spout {
                sender_name: sender.name.clone(),
            },
            fps: 30,
            bitrate: 3_000_000,
            start_qpc_ns: 0,
            audio_offset_ns,
        },
    )?;
    let origin = recorder.status().origin_qpc_ns;
    sender.arm(origin)?;
    let sink = recorder.audio_sink();
    let producer = thread::spawn(move || -> anyhow::Result<(usize, u64, u64)> {
        let mut first = 0usize;
        let mut packets = 0u64;
        let mut max_lateness = 0u64;
        // Advanced audio needs real source samples after the video cutoff.
        let source_frames = SECONDS * RATE
            + ((-audio_offset_ns).max(0) as i128 * RATE as i128 / 1_000_000_000) as usize;
        while first < source_frames {
            let count = BLOCK.min(source_frames - first);
            let samples = (first..first + count)
                .flat_map(|frame| {
                    let sample = pulse_sample(frame);
                    [sample, sample]
                })
                .collect::<Vec<_>>();
            let source_ns = origin + first as i64 * 1_000_000_000 / RATE as i64;
            let delivery_ns = source_ns + AUDIO_DELIVERY_DELAY_NS;
            while vsm_video::now_ns() < delivery_ns {
                thread::sleep(Duration::from_millis(1));
            }
            max_lateness = max_lateness.max(vsm_video::now_ns().saturating_sub(delivery_ns) as u64);
            sink.push(&samples, source_ns)?;
            first += count;
            packets += 1;
        }
        Ok((first, packets, max_lateness))
    });
    // stop_at fixes the 8 s cutoff and drains the delayed processing tail.
    let record_result = recorder.stop_at(origin + SECONDS as i64 * 1_000_000_000);
    let audio_result = producer
        .join()
        .map_err(|_| anyhow::anyhow!("AV audio producer panicked"))?;
    let sender_result = sender.finish();
    let report = record_result?;
    let (audio_produced, audio_packets, max_audio_lateness_ns) = audio_result?;
    let sender_report = sender_result?;
    let pulses = (1..=6).map(|index| {
        format!("{{\"index\":{index},\"sample\":{},\"intended_qpc_ns\":{},\"duration_frames\":{PULSE_FRAMES}}}",
            index * RATE, json_string(&(origin + index as i64 * 1_000_000_000).to_string()))
    }).collect::<Vec<_>>().join(",");
    let flashes = sender_report.flashes.iter().map(|event| {
        format!("{{\"index\":{},\"white\":{},\"intended_qpc_ns\":{},\"selected_qpc_ns\":{},\"publish_begin_qpc_ns\":{},\"published_qpc_ns\":{},\"access_wait_ns\":{}}}",
            event.index, event.white, json_string(&event.intended_qpc_ns.to_string()),
            json_string(&event.selected_qpc_ns.to_string()), json_string(&event.publish_begin_qpc_ns.to_string()),
            json_string(&event.published_qpc_ns.to_string()), event.access_wait_ns)
    }).collect::<Vec<_>>().join(",");
    let recorder_json = format!(
        "{{\"video_frames\":{},\"audio_frames\":{},\"repeated_frames\":{},\"skipped_frames\":{},\"peak_audio_queue_frames\":{},\"max_video_processing_ns\":{},\"max_audio_jitter_frames\":{},\"audio_clock_adjustment_frames\":{},\"audio_offset_ns\":{},\"origin_qpc_ns\":{},\"end_qpc_ns\":{},\"warning\":{}}}",
        report.video_frames,
        report.audio_frames,
        report.repeated_frames,
        report.skipped_frames,
        report.peak_audio_queue_frames,
        report.max_video_processing_ns,
        report.max_audio_jitter_frames,
        report.audio_clock_adjustment_frames,
        report.audio_offset_ns,
        json_string(&report.origin_qpc_ns.to_string()),
        json_string(&report.end_qpc_ns.to_string()),
        report
            .warning
            .as_deref()
            .map(json_string)
            .unwrap_or_else(|| "null".into())
    );
    let json = format!(
        "{{\"schema\":\"vsm-av-fixture-1\",\"scenario\":{},\"mp4\":{},\"recording_origin_qpc_ns\":{},\"duration_seconds\":{SECONDS},\"video_fps\":30,\"audio_rate\":{RATE},\"audio_channels\":2,\"pulse_hz\":1000,\"pulse_amplitude\":0.4,\"pulse_frames\":{PULSE_FRAMES},\"flash_duration_ns\":100000000,\"sender_mutex_hold_ms\":{hold_ms},\"audio_delivery_delay_ns\":{AUDIO_DELIVERY_DELAY_NS},\"audio_produced_frames\":{audio_produced},\"audio_packets\":{audio_packets},\"max_audio_delivery_lateness_ns\":{max_audio_lateness_ns},\"sender_updates\":{},\"audio_pulses\":[{pulses}],\"source_flash_events\":[{flashes}],\"recorder\":{recorder_json}}}\n",
        json_string(&scenario),
        json_string(&video_path.to_string_lossy()),
        json_string(&origin.to_string()),
        sender_report.updates
    );
    fs::write(&json_path, json)?;
    println!(
        "scenario={scenario} origin_qpc_ns={origin} video_frames={} audio_frames={} json={}",
        report.video_frames,
        report.audio_frames,
        json_path.display()
    );
    anyhow::ensure!(
        report.video_frames > 0 && report.video_frames <= (SECONDS * 30 + 1) as u64,
        "No video or video frame count exceeds the 30 fps cutoff"
    );
    anyhow::ensure!(
        report.origin_qpc_ns == origin
            && report.end_qpc_ns - origin == SECONDS as i64 * 1_000_000_000,
        "Recorder origin/cutoff differs from the shared 8 s timeline"
    );
    anyhow::ensure!(
        report.audio_frames == (SECONDS * RATE) as u64,
        "Audio cutoff differs from 384000 frames"
    );
    Ok(json_path.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let output = std::path::Path::new(args.get(1).ok_or_else(|| {
        anyhow::anyhow!(
            "Usage: video-av-check <new-output-directory> [all|0|20|60] [audio-offset-ms]"
        )
    })?);
    std::fs::create_dir_all(output)?;
    let modes = match args.get(2).map(String::as_str).unwrap_or("all") {
        "all" => vec![0, 20, 60],
        "0" => vec![0],
        "20" => vec![20],
        "60" => vec![60],
        _ => anyhow::bail!("Mutex scenario must be all, 0, 20 or 60"),
    };
    let offset_ms: i64 = args.get(3).map_or(Ok(0), |value| value.parse())?;
    anyhow::ensure!(
        (-2000..=2000).contains(&offset_ms),
        "Audio offset must be -2000 to +2000 ms"
    );
    for hold in modes {
        run(output, hold, offset_ms * 1_000_000)?;
    }
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("video-av-check requires Windows");
}
