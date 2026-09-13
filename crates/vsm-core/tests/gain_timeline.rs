mod common;
use anyhow::Result;
use serde_json::json;
use vsm_core::{
    dsp::Level,
    processor::{AudioPacket, Processor},
    session,
};

fn packet(frame: u64) -> AudioPacket {
    AudioPacket {
        samples: vec![0.; 512],
        frames: 256,
        channels: 2,
        flags: 0,
        device: frame,
        file: frame,
        time: (frame * 1_000_000_000 / 48000) as i64,
    }
}
fn close(value: f64, expected: f64) {
    assert!((value - expected).abs() < 1e-6, "{value} != {expected}");
}

#[test]
fn gain_events_checkpoints_and_override() -> Result<()> {
    let mut config = session::defaults();
    config["gain_db"] = json!(-30.);
    let mut p = Processor::new(&config, 0, &common::library(), None)?;
    p.event(json!({"kind":"gain_change","gain_db":-6.,"receive_time_ns":5_000_000}))?;
    close(
        p.push(&packet(0), 0)?[0].pose["gain_db"].as_f64().unwrap(),
        -30.,
    );
    let checkpoint = p.checkpoint();
    let mut q = Processor::new(&session::defaults(), 0, &common::library(), None)?;
    q.restore(&checkpoint)?;
    close(q.checkpoint()["gain_db"].as_f64().unwrap(), -30.);
    let changed = q.push(&packet(256), 0)?;
    close(changed[0].pose["gain_db"].as_f64().unwrap(), -6.);
    close(
        changed[0].pose["gain_from_linear"].as_f64().unwrap(),
        10f64.powf(-30. / 20.),
    );
    let mut restored = Processor::new(&config, 0, &common::library(), None)?;
    restored.restore(&q.checkpoint())?;
    close(
        restored.push(&packet(512), 0)?[0].pose["gain_db"]
            .as_f64()
            .unwrap(),
        -6.,
    );
    let mut fixed = Processor::new(&config, 0, &common::library(), None)?;
    fixed.override_gain(-18.)?;
    fixed.restore(&checkpoint)?;
    close(
        fixed.push(&packet(256), 0)?[0].pose["gain_db"]
            .as_f64()
            .unwrap(),
        -18.,
    );
    Ok(())
}

#[test]
fn preview_changes_ramp_both_channels_and_keep_limiter() -> Result<()> {
    let mut level = Level::default();
    let mut pcm = [0.5; 8];
    level.apply_ramp(&mut pcm, 0.1, 0.5)?;
    for (pair, expected) in pcm.chunks_exact(2).zip([0.1, 0.15, 0.2, 0.25]) {
        close(pair[0] as f64, expected);
        close(pair[1] as f64, expected);
    }
    let mut hot = [10.; 512];
    level.apply_ramp(&mut hot, 0.5, 1.)?;
    assert!(hot.iter().all(|v| v.abs() <= 0.980001));
    assert!(level.apply_ramp(&mut hot, f64::NAN, 1.).is_err());
    Ok(())
}
