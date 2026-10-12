//! Developer-only native encoder soak test, including realistic QPC jitter.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::{path::Path, thread, time::Duration};
    use vsm_video::{Recorder, VideoConfig, VideoSource};
    let args: Vec<_> = std::env::args().collect();
    let path = Path::new(args.get(1).ok_or_else(|| {
        anyhow::anyhow!(
            "record-soak <new.mp4> <seconds> [synthetic|sender] [width] [height] [jitter-ns]"
        )
    })?);
    let seconds: u64 = args.get(2).map_or(Ok(60), |v| v.parse())?;
    anyhow::ensure!(
        (1..=86_400).contains(&seconds),
        "Duration must be 1–86400 seconds"
    );
    let name = args.get(3).map(String::as_str).unwrap_or("synthetic");
    let width = args.get(4).map_or(Ok(3840), |v| v.parse())?;
    let height = args.get(5).map_or(Ok(2160), |v| v.parse())?;
    let jitter: i64 = args.get(6).map_or(Ok(0), |v| v.parse())?;
    anyhow::ensure!(
        (0..=1_000_000_000).contains(&jitter),
        "Jitter outside test bounds"
    );
    let source = if name == "synthetic" {
        VideoSource::Synthetic { width, height }
    } else {
        VideoSource::Spout {
            sender_name: name.into(),
        }
    };
    let mut recorder = Recorder::start(
        path,
        VideoConfig {
            source,
            fps: 30,
            bitrate: 8_000_000,
            start_qpc_ns: 0,
            audio_offset_ns: 0,
        },
    )?;
    let origin = recorder.status().origin_qpc_ns;
    let end = origin + seconds as i64 * 1_000_000_000;
    let sink = recorder.audio_sink();
    let producer = thread::spawn(move || -> anyhow::Result<()> {
        let tones: Vec<f32> = (0..48)
            .map(|n| (n as f64 * std::f64::consts::TAU / 48.).sin() as f32 * 0.15)
            .collect();
        let mut first = 0u64;
        while first < seconds * 48_000 {
            let count = 256.min(seconds * 48_000 - first);
            let at = origin + (first as i128 * 1_000_000_000 / 48_000) as i64;
            while vsm_video::now_ns() < at + 100_000_000 {
                thread::sleep(Duration::from_millis(1));
            }
            let mut samples = Vec::with_capacity(count as usize * 2);
            for frame in first..first + count {
                samples.extend([tones[(frame % 48) as usize]; 2]);
            }
            let noise = if first == 0 {
                0
            } else {
                ((first / 256 * 37) % 97) as i64 * jitter / 48 - jitter
            };
            sink.push(&samples, at + noise)?;
            first += count;
        }
        Ok(())
    });
    let mut next_status = origin;
    while vsm_video::now_ns() < end {
        thread::sleep(Duration::from_millis(50));
        let now = vsm_video::now_ns();
        if now >= next_status {
            let status = recorder.status();
            println!(
                "elapsed_s={} status={status:?}",
                (now - origin) / 1_000_000_000
            );
            anyhow::ensure!(
                status.active && status.error.is_none(),
                "Recorder stopped during soak: {status:?}"
            );
            next_status = now + 10_000_000_000;
        }
    }
    let report = recorder.stop_at(end)?;
    producer
        .join()
        .map_err(|_| anyhow::anyhow!("Audio fixture producer panicked"))??;
    anyhow::ensure!(
        report.audio_frames == seconds * 48_000,
        "Audio length differs: {report:?}"
    );
    println!("report={report:?}");
    println!("inspect={:?}", vsm_video::inspect(path)?);
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("record-soak requires Windows");
}
