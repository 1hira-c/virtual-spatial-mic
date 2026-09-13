#[path = "../../vsm-core/tests/common/mod.rs"]
mod common;
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use vsm_core::{
    session,
    wave::{self, WaveReader},
};
use vsm_live::engine::Engine;

fn exercise(base: &Path, raw: bool, fail: bool) -> Result<Value> {
    fs::create_dir(base)?;
    let events = base.join("fixture.events.jsonl");
    common::events(&events, 30)?;
    let destination = base.join("takes");
    if fail {
        fs::write(&destination, b"blocked by fixture")?;
    } else {
        fs::create_dir(&destination)?;
    }
    let mut config = session::defaults();
    config["fixture"] = json!(true);
    config["fixture_events_path"] = json!(events);
    config["output_root"] = json!(destination);
    config["preroll_seconds"] = json!(1.);
    config["save_raw"] = json!(raw);
    let samples = Arc::new(Mutex::new(Vec::new()));
    let collected = samples.clone();
    let mut engine = Engine::start(
        config,
        common::library(),
        Arc::new(move |audio, _| {
            collected.lock().unwrap().extend_from_slice(audio);
            Ok(())
        }),
    )?;
    thread::sleep(Duration::from_millis(2100));
    if !fail {
        assert_eq!(fs::read_dir(&destination)?.count(), 0);
    }
    assert!(samples.lock().unwrap().len() > 48000);
    let levels = engine.audio_levels();
    assert!(
        (levels[0] - 0.1).abs() < 0.001,
        "selected input peak: {levels:?}"
    );
    assert!(
        levels[1] > 0. && levels[2] > 0.,
        "processed stereo: {levels:?}"
    );
    engine.command(json!({"event":"recording_started","source":"studio"}))?;
    thread::sleep(Duration::from_millis(1100));
    engine.command(json!({"event":"recording_paused"}))?;
    thread::sleep(Duration::from_millis(100));
    engine.command(json!({"event":"recording_unpaused"}))?;
    thread::sleep(Duration::from_millis(200));
    engine.command(json!({"event":"recording_stopped","video_path":"first.mp4"}))?;
    if !fail {
        thread::sleep(Duration::from_millis(30));
        engine.command(json!({"event":"recording_started","source":"obs"}))?;
        thread::sleep(Duration::from_millis(800));
        engine.command(json!({"event":"recording_stopped","video_path":"second.mp4"}))?;
    }
    thread::sleep(Duration::from_millis(1000));
    let before = samples.lock().unwrap().len();
    thread::sleep(Duration::from_millis(250));
    assert!(samples.lock().unwrap().len() > before);
    engine.join();
    assert_eq!(engine.audio_levels(), [0.; 3]);
    let state = engine.status();
    assert_eq!(state["state"], "stopped", "{state}");
    if fail {
        assert_eq!(state["archive_state"], "failed");
        return Ok(json!({"disk_failure_kept_monitoring":true}));
    }
    let rows = session::list(&destination)?;
    assert_eq!(rows.as_array().unwrap().len(), 2);
    let monitor = samples.lock().unwrap();
    for row in rows.as_array().unwrap() {
        let path = Path::new(row["directory"].as_str().unwrap());
        let report = wave::load_json(&path.join("session.json"))?;
        let name = path.file_name().unwrap().to_str().unwrap();
        let started = chrono::DateTime::parse_from_rfc3339(
            report["recording_started_local"].as_str().unwrap(),
        )?;
        assert_eq!(name, started.format("%Y%m%d-%H%M%S").to_string());
        assert_eq!(name.len(), 15);
        assert_eq!(report["status"], "stopped");
        assert_eq!(report["stop_reason"], "obs_recording_stopped");
        assert!(report["actual_preroll_seconds"].as_f64().unwrap() > 0.8);
        let mut audio = WaveReader::open(&path.join("live-output.wav"))?;
        let pcm = audio.read(0, audio.frames as u32)?;
        if raw {
            assert!(path.join("raw/pose-bootstrap.json").is_file());
            let mut poses = session::Lines::open(&path.join("applied-poses.jsonl"))?;
            let mut covered = 0;
            while let Some(p) = poses.next.take() {
                let a = p["engine_output_file_frame"].as_u64().unwrap() as usize;
                let b = p["output_file_frame"].as_u64().unwrap() as usize;
                let n = p["valid_frames"].as_u64().unwrap() as usize;
                assert_eq!(&pcm[b * 2..(b + n) * 2], &monitor[a * 2..(a + n) * 2]);
                covered += n;
                poses.advance()?;
            }
            assert_eq!(covered, audio.frames as usize);
            let rendered = session::reprocess(
                path,
                &json!({}),
                &std::sync::atomic::AtomicBool::new(false),
                &common::library(),
                |_| {},
                false,
            )?;
            assert_eq!(rendered["status"], "complete");
            assert_eq!(rendered["checkpoint_bridge_present"], true);
        } else {
            assert!(!path.join("raw").exists());
            assert!(!path.join("applied-poses.jsonl").exists());
            assert_eq!(row["can_reprocess"], false);
        }
    }
    Ok(
        json!({"save_raw":raw,"takes":2,"idle_disk_files":0,"monitor_continued":true,"live_pcm_matches_saved":raw,"engine":state}),
    )
}
#[test]
fn ring_recording_and_disk_failure() -> Result<()> {
    let base = common::output("ring");
    let report = json!({"raw_on":exercise(&base.join("raw-on"),true,false)?,"raw_off":exercise(&base.join("raw-off"),false,false)?,"disk_failure":exercise(&base.join("disk-failure"),true,true)?});
    wave::save_json(&base.join("report.json"), &report)?;
    println!("Rust ring evidence: {}", base.display());
    Ok(())
}
