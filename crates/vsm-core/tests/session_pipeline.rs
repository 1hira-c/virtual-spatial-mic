mod common;
use anyhow::Result;
use serde_json::json;
use std::{fs, sync::atomic::AtomicBool};
use vsm_core::{
    dsp::Binaural,
    session,
    wave::{self, WaveReader},
};

#[test]
fn reprocess_from_originals_and_protect_latest() -> Result<()> {
    let base = common::output("session");
    let root = common::fixture(&base)?;
    assert!(session::inspect(&root)?["name"].is_string());
    let library = common::library();
    let cancel = AtomicBool::new(false);
    let before = wave::sha256(&root.join("raw/microphone.wav"))?;
    let render = |options: &serde_json::Value| {
        session::reprocess(&root, options, &cancel, &library, |_| {}, true)
    };
    let first = render(&json!({}))?;
    let mut reader =
        WaveReader::open(std::path::Path::new(first["output_path"].as_str().unwrap()))?;
    assert_eq!(reader.frames, 95520);
    let a = reader.read(0, reader.frames as u32)?;
    assert!(a.iter().any(|v| v.abs() > 0.001));
    let mut lines = session::Lines::open(
        &std::path::Path::new(first["directory"].as_str().unwrap()).join("applied-poses.jsonl"),
    )?;
    let (mut left, mut right, mut early) = (false, false, false);
    while let Some(p) = lines.next.take() {
        if p["source_time_ns"].as_i64().unwrap() < 100_000_000 {
            early |= p["valid"] == false;
        }
        if let Some(x) = p["relative_m"][0].as_f64() {
            left |= x < -0.7;
            right |= x > 0.7;
        }
        lines.advance()?;
    }
    assert!(left && right && early);
    let second = render(&json!({"gain_db":-24.020599913279624}))?;
    let mut b = WaveReader::open(std::path::Path::new(
        second["output_path"].as_str().unwrap(),
    ))?;
    let b = b.read(0, b.frames as u32)?;
    assert_eq!(a.len(), b.len());
    assert!(a.iter().zip(b).all(|(a, b)| (a * 0.5 - b).abs() < 1e-6));
    let third = render(&json!({"channel":1}))?;
    let mut c = WaveReader::open(std::path::Path::new(third["output_path"].as_str().unwrap()))?;
    let c = c.read(0, c.frames as u32)?;
    assert_ne!(a, c);
    assert_eq!(before, wave::sha256(&root.join("raw/microphone.wav"))?);
    let latest = wave::load_json(&root.join("renders/latest-native.json"))?;
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(render(&json!({})).is_err());
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        latest,
        wave::load_json(&root.join("renders/latest-native.json"))?
    );
    assert!(render(&json!({"gain_db":12})).is_err());
    let raw_off = base.join("raw-off");
    fs::create_dir(&raw_off)?;
    wave::save_json(
        &raw_off.join("session.json"),
        &json!({"kind":"native_obs_ring_take","status":"stopped"}),
    )?;
    fs::copy(
        first["output_path"].as_str().unwrap(),
        raw_off.join("live-output.wav"),
    )?;
    assert_eq!(session::inspect(&raw_off)?["can_reprocess"], false);
    assert!(session::audio_path(&raw_off, false)?.is_file());
    assert!(session::reprocess(&raw_off, &json!({}), &cancel, &library, |_| {}, true).is_err());
    assert!(session::child(&root, "../outside").is_err());
    wave::save_json(
        &root.join("renders/latest-native.json"),
        &json!({"output":"../escape.wav"}),
    )?;
    assert!(session::audio_path(&root, false).is_err());
    let settings = session::defaults();
    session::save_settings(&base.join("settings/user.json"), &settings)?;
    assert_eq!(
        settings,
        session::load_settings(&base.join("settings/user.json"))?
    );
    // Legacy WASAPI timestamp-error rows used null. Preserve the same discarded
    // packet and sample output instead of rejecting the whole recording.
    let clock_path = root.join("raw/audio.clock.jsonl");
    let clocks = fs::read_to_string(&clock_path)?;
    let mut rewritten = String::new();
    for line in clocks.lines() {
        let mut row: serde_json::Value = serde_json::from_str(line)?;
        if row["flags"] == 4 {
            row["recorded_time_ns"] = serde_json::Value::Null;
        }
        rewritten.push_str(&row.to_string());
        rewritten.push('\n');
    }
    fs::write(clock_path, rewritten)?;
    let result = render(&json!({}))?;
    let mut restored = WaveReader::open(std::path::Path::new(
        result["output_path"].as_str().unwrap(),
    ))?;
    assert_eq!(a, restored.read(0, restored.frames as u32)?);
    Ok(())
}
#[test]
fn sdk_rejection_keeps_history_and_drain_finishes() -> Result<()> {
    let library = common::library();
    let mut a = Binaural::new(&library, 256, true)?;
    let mut b = Binaural::new(&library, 256, true)?;
    let mut input = [0.; 256];
    input[0] = 0.1;
    let (mut out, mut expected) = ([0.; 512], [0.; 512]);
    a.process(&input, &mut out, [1., 0., -1.])?;
    b.process(&input, &mut expected, [1., 0., -1.])?;
    assert_eq!(out, expected);
    let mut bad = input;
    bad[12] = f32::NAN;
    assert!(a.process(&bad, &mut out, [1., 0., -1.]).is_err());
    assert!(out.iter().all(|v| *v == 0.));
    a.process(&input, &mut out, [1., 0., -1.])?;
    b.process(&input, &mut expected, [1., 0., -1.])?;
    assert_eq!(out, expected);
    let mut frames = 0;
    loop {
        let (n, done) = a.drain(&mut out)?;
        frames += n;
        assert!(frames < 48000);
        if done {
            break;
        }
    }
    assert!(frames >= 256);
    assert!(a.process(&input, &mut out, [1., 0., -1.]).is_err());
    a.reset();
    a.process(&input, &mut out, [1., 0., -1.])?;
    Ok(())
}
