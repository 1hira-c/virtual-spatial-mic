#[path = "../../vsm-core/tests/common/mod.rs"]
mod common;
use anyhow::Result;
use serde_json::json;
use std::{
    fs,
    sync::{Arc, mpsc},
    time::Duration,
};
use vsm_live::{clock, ring::Ring};

#[test]
fn collision_keeps_existing_take_and_allows_next_start() -> Result<()> {
    let root = common::output("naming");
    let origin = clock::now_ns();
    let wall = chrono::Local::now();
    // Fix the first event in the middle of an earlier second, independent of
    // test scheduling or the actual writer's start time.
    let elapsed = i64::from(wall.timestamp_subsec_nanos()) + 4_500_000_000;
    let earlier = wall - chrono::Duration::nanoseconds(elapsed);
    let occupied = root.join(earlier.format("%Y%m%d-%H%M%S").to_string());
    fs::create_dir(&occupied)?;
    fs::write(occupied.join("session.json"), b"unchanged original")?;
    fs::write(occupied.join("live-output.wav"), b"unchanged audio")?;
    let (send, receive) = mpsc::channel();
    let mut ring = Ring::new(
        json!({"output_root":root,"save_raw":false}),
        origin,
        Arc::new(move |value| {
            let _ = send.send(value);
        }),
    )?;
    ring.command(json!({"event":"recording_started","source":"studio","qpc_ns":origin-elapsed}))?;
    let status = receive.recv_timeout(Duration::from_secs(5))?;
    assert_eq!(status["archive_state"], "failed", "{status}");
    assert!(
        status["archive_error"]
            .as_str()
            .unwrap()
            .contains("同じ録音開始日時")
    );
    ring.poll(clock::now_ns() - origin);
    assert_eq!(ring.stats()["recording"], false);
    assert!(!ring.gain_locked());
    assert_eq!(
        fs::read(occupied.join("session.json"))?,
        b"unchanged original"
    );
    assert_eq!(
        fs::read(occupied.join("live-output.wav"))?,
        b"unchanged audio"
    );
    ring.command(json!({"event":"recording_started","source":"obs","qpc_ns":clock::now_ns()}))?;
    let status = receive.recv_timeout(Duration::from_secs(5))?;
    assert_eq!(status["archive_state"], "recording", "{status}");
    let path = std::path::Path::new(status["recording_directory"].as_str().unwrap());
    let report = vsm_core::wave::load_json(&path.join("session.json"))?;
    let wall =
        chrono::DateTime::parse_from_rfc3339(report["recording_started_local"].as_str().unwrap())?;
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        wall.format("%Y%m%d-%H%M%S").to_string()
    );
    ring.finish("test_finished");
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(5))?["archive_state"],
        "saved"
    );
    assert_eq!(fs::read_dir(root)?.count(), 2);
    Ok(())
}
