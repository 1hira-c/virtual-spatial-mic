#![allow(dead_code)]
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use vsm_core::{
    session,
    wave::{self, WaveWriter},
};
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
pub fn library() -> PathBuf {
    if let Some(path) = std::env::var_os("VSM_TEST_STEAM_AUDIO") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "Configured Steam Audio test library is missing"
        );
        return path;
    }
    root().join(if cfg!(windows) {
        ".deps/steam-audio-4.8.1/steamaudio/lib/windows-x64/phonon.dll"
    } else {
        ".deps/steam-audio-4.8.1/steamaudio/lib/linux-x64/libphonon.so"
    })
}
pub fn output(prefix: &str) -> PathBuf {
    let path = root().join("out/rust-tests").join(format!(
        "{prefix}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn snapshot(address: &str, value: Value, time: i64, kind: &str) -> Value {
    json!({"kind":"snapshot","address":address,"receive_time_ns":time,"request_start_ns":0.max(time-1),"body":json!({"FULL_PATH":address,"TYPE":kind,"ACCESS":1,"VALUE":[value]}).to_string()})
}
fn camera(time: i64) -> Value {
    let mut b = Vec::new();
    for s in ["/usercamera/Pose", ",ffffff"] {
        b.extend(s.as_bytes());
        b.push(0);
        while b.len() % 4 != 0 {
            b.push(0);
        }
    }
    for v in [0f32, 1.6, 0., 0., 0., 0.] {
        b.extend(v.to_bits().to_be_bytes());
    }
    json!({"kind":"udp","receive_time_ns":time,"datagram_base64":STANDARD.encode(&b)})
}
pub fn events(path: &Path, seconds: usize) -> Result<()> {
    let mut f = File::create(path)?;
    writeln!(
        f,
        "{}",
        json!({"kind":"udp","receive_time_ns":0,"datagram_base64":"AAAA"})
    )?;
    writeln!(
        f,
        "{}",
        snapshot("/avatar/parameters/VBS/Ref/ProbeVersion", json!(2), 0, "i")
    )?;
    writeln!(f, "{}", camera(100_000_000))?;
    for i in 0..seconds * 10 {
        for (axis, name) in ["x", "y", "z"].iter().enumerate() {
            let value: f64 = match axis {
                0 => {
                    if (i / 10) % 2 == 0 {
                        0.75
                    } else {
                        -0.75
                    }
                }
                1 => 1.6,
                _ => 0.5,
            };
            let prefix = format!("/avatar/parameters/VBS/Ref/mouth/p/{name}");
            let at = (i + 1) as i64 * 100_000_000;
            writeln!(
                f,
                "{}",
                snapshot(&prefix, json!(1. - value.abs() / 1000.), at, "f")
            )?;
            writeln!(
                f,
                "{}",
                snapshot(
                    &(prefix + "+"),
                    json!(if value > 0. { 2. * value / 1000. } else { 0. }),
                    at,
                    "f"
                )
            )?;
        }
    }
    Ok(())
}
pub fn fixture(base: &Path) -> Result<PathBuf> {
    let root = base.join("synthetic-take");
    fs::create_dir_all(root.join("raw"))?;
    let mut wave = WaveWriter::create(&root.join("raw/microphone.wav"), 2)?;
    let samples: Vec<f32> = (0..96000)
        .flat_map(|i| {
            [
                (0.08 * (i as f64 * 0.03).sin()) as f32,
                (0.04 * (i as f64 * 0.051).sin()) as f32,
            ]
        })
        .collect();
    wave.append(&samples)?;
    wave.finish()?;
    let mut clock = File::create(root.join("raw/audio.clock.jsonl"))?;
    for i in 0..200 {
        writeln!(
            clock,
            "{}",
            json!({"file_frame_start":i*480,"frames":480,"device_position_frames":i*480,"flags":if i==80{4}else{0},"recorded_time_ns":i*10_000_000})
        )?;
    }
    events(&root.join("raw/osc.events.jsonl"), 2)?;
    wave::save_json(
        &root.join("session.json"),
        &json!({"kind":"native_obs_ring_take","schema_version":"0.2.0","status":"stopped","config":session::defaults()}),
    )?;
    Ok(root)
}
