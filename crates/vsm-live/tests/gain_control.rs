#[path = "../../vsm-core/tests/common/mod.rs"]
mod common;
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
    thread,
    time::Duration,
};
use vsm_core::{session, wave};
use vsm_live::engine::Engine;

fn gains(path: &Path) -> Result<Vec<f64>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["gain_db"]
                .as_f64()
                .unwrap()
        })
        .collect())
}
fn has(values: &[f64], gain: f64) -> bool {
    values.iter().any(|v| (v - gain).abs() < 1e-6)
}

#[test]
fn preview_gain_recording_lock_and_replay() -> Result<()> {
    let base = common::output("gain-control");
    let events = base.join("events.jsonl");
    common::events(&events, 30)?;
    let mut config = session::defaults();
    config["fixture"] = json!(true);
    config["fixture_events_path"] = json!(events);
    config["output_root"] = json!(base.join("takes"));
    config["preroll_seconds"] = json!(2.);
    config["gain_db"] = json!(-30.);
    let output = Arc::new(Mutex::new(Vec::<f32>::new()));
    let collected = output.clone();
    let mut engine = Engine::start(
        config,
        common::library(),
        Arc::new(move |pcm, _| {
            collected.lock().unwrap().extend_from_slice(pcm);
            Ok(())
        }),
    )?;
    thread::sleep(Duration::from_millis(600));
    let quiet = engine.audio_levels()[1];
    assert!(quiet > 0.);
    let before = output.lock().unwrap().len();
    for invalid in [f64::NAN, f64::INFINITY, -61., 1.] {
        assert!(engine.set_gain(invalid).is_err());
    }
    engine.set_gain(-6.)?;
    assert_eq!(engine.status()["gain_db"], -6.);
    thread::sleep(Duration::from_millis(400));
    let loud = engine.audio_levels()[1];
    assert!(
        loud > quiet * 5.,
        "live gain did not reach the output: {quiet} -> {loud}"
    );
    assert!(output.lock().unwrap().len() > before);
    engine.command(json!({"event":"recording_started"}))?;
    // Enqueued immediately after Start: must be rejected even before UI polling.
    assert!(engine.set_gain(-24.).is_err());
    thread::sleep(Duration::from_millis(450));
    assert_eq!(engine.status()["recording"], true);
    assert_eq!(engine.status()["gain_db"], -6.);
    engine.command(json!({"event":"recording_stopped"}))?;
    assert!(
        engine.set_gain(-24.).is_err(),
        "the recorded tail must stay locked"
    );
    thread::sleep(Duration::from_millis(850));
    engine.set_gain(-12.)?;
    engine.join();
    assert!(engine.set_gain(-18.).is_err());
    let state = engine.status();
    assert_eq!(state["state"], "stopped", "{state}");
    let rows = session::list(&base.join("takes"))?;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let take = Path::new(rows[0]["directory"].as_str().unwrap());
    let manifest = wave::load_json(&take.join("session.json"))?;
    assert_eq!(manifest["config"]["gain_db"], -6.);
    let live = gains(&take.join("applied-poses.jsonl"))?;
    assert!(has(&live, -30.) && has(&live, -6.));
    assert!(!has(&live, -24.) && !has(&live, -12.));
    let events = fs::read_to_string(take.join("raw/osc.events.jsonl"))?;
    assert!(events.contains("gain_change"));
    let replay = session::reprocess(
        take,
        &json!({}),
        &AtomicBool::new(false),
        &common::library(),
        |_| {},
        true,
    )?;
    assert_eq!(replay["status"], "complete");
    // Locate the output through the public audio-path API; no private filename assumptions.
    let rendered = session::audio_path(take, false)?;
    let replay_gains = gains(&rendered.parent().unwrap().join("applied-poses.jsonl"))?;
    assert!(has(&replay_gains, -30.) && has(&replay_gains, -6.));
    session::reprocess(
        take,
        &json!({"gain_db":-18.}),
        &AtomicBool::new(false),
        &common::library(),
        |_| {},
        true,
    )?;
    let rendered = session::audio_path(take, false)?;
    let fixed = gains(&rendered.parent().unwrap().join("applied-poses.jsonl"))?;
    assert!(!fixed.is_empty() && fixed.iter().all(|v| (v + 18.).abs() < 1e-6));
    println!("Gain control evidence: {}", base.display());
    Ok(())
}
