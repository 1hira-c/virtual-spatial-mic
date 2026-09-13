use crate::{
    pose::LivePose,
    processor::{AudioPacket, ProcessedBlock, Processor},
    wave::{WaveReader, WaveWriter, load_json, save_json},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn defaults() -> Value {
    json!({"schema_version":1,"endpoint_id":"","output_root":"","channel":0,"gain_db":-18.,"save_raw":true,"source_mode":"auto","mouth_offset_m":[0.,0.0064,-0.0736],"monitor_endpoint":""})
}
pub fn settings(value: &Value) -> Result<Value> {
    ensure!(value.is_object(), "Invalid settings object");
    let mut out = defaults();
    for key in [
        "endpoint_id",
        "output_root",
        "channel",
        "gain_db",
        "save_raw",
        "source_mode",
        "mouth_offset_m",
        "monitor_endpoint",
    ] {
        if let Some(v) = value.get(key) {
            out[key] = v.clone();
        }
    }
    let gain = out["gain_db"].as_f64().context("Invalid gain")?;
    let channel = out["channel"].as_u64().context("Invalid input channel")?;
    ensure!(
        gain.is_finite() && (-60.0..=0.0).contains(&gain) && channel <= 31,
        "Invalid recording settings"
    );
    for key in [
        "endpoint_id",
        "output_root",
        "source_mode",
        "monitor_endpoint",
    ] {
        ensure!(out[key].is_string(), "Invalid setting: {key}");
    }
    ensure!(out["save_raw"].is_boolean(), "Invalid raw-save setting");
    LivePose::new(
        serde_json::from_value(out["mouth_offset_m"].clone())?,
        out["source_mode"].as_str().unwrap(),
    )?;
    Ok(out)
}
pub fn load_settings(path: &Path) -> Result<Value> {
    if path.exists() {
        settings(&load_json(path)?)
    } else {
        Ok(defaults())
    }
}
pub fn save_settings(path: &Path, value: &Value) -> Result<()> {
    let normalized = settings(value)?;
    fs::create_dir_all(path.parent().context("Settings directory missing")?)?;
    let temp = path.with_extension("json.pending");
    save_json(&temp, &normalized)?;
    if path.exists() {
        fs::copy(path, path.with_extension("json.previous"))?;
    }
    fs::copy(&temp, path)?;
    fs::remove_file(temp)?;
    Ok(())
}
pub fn child(root: &Path, name: &str) -> Result<PathBuf> {
    let base = root.canonicalize()?;
    let rel = Path::new(name);
    ensure!(
        !rel.as_os_str().is_empty()
            && rel
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "Session path escapes recording"
    );
    let mut path = base.clone();
    for component in rel.components() {
        path.push(component);
        if path.exists() {
            let resolved = path.canonicalize()?;
            ensure!(
                resolved.starts_with(&base),
                "Session symlink escapes recording"
            );
            path = resolved;
        }
    }
    Ok(path)
}
fn manifest(root: &Path) -> Result<Value> {
    load_json(&child(
        root,
        if root.join("session.json").exists() {
            "session.json"
        } else {
            "take.json"
        },
    )?)
}
pub fn audio_path(root: &Path, original: bool) -> Result<PathBuf> {
    if original {
        let p = child(root, "raw/microphone.wav")?;
        ensure!(p.is_file(), "原音は保存されていません");
        return Ok(p);
    }
    let latest = child(root, "renders/latest-native.json")?;
    if latest.is_file() {
        let v = load_json(&latest)?;
        let p = child(
            root,
            v["output"].as_str().context("Invalid latest audio path")?,
        )?;
        if p.is_file() {
            return Ok(p);
        }
    }
    let p = child(root, "live-output.wav")?;
    ensure!(p.is_file(), "再処理すると試聴できます");
    Ok(p)
}
pub fn inspect(root: &Path) -> Result<Value> {
    let root = root.canonicalize()?;
    let m = manifest(&root)?;
    let kind = m["kind"].as_str().unwrap_or("");
    let status = m["status"].as_str().unwrap_or("");
    let supported = [
        "native_obs_ring_take",
        "native_obs_live_session",
        "vr_diagnostic_take",
    ]
    .contains(&kind);
    let stopped = !["recording", "initializing", "running"].contains(&status);
    let raw = child(&root, "raw/microphone.wav")?.is_file();
    let logs = child(&root, "raw/audio.clock.jsonl")?.is_file()
        && (child(&root, "raw/osc.events.jsonl")?.is_file()
            || child(&root, "raw/osc.jsonl")?.is_file());
    let message = if !stopped {
        "録音停止後に再処理できます"
    } else if !raw {
        "原音なし：処理済み音声のみ試聴できます"
    } else if !supported || !logs {
        "再処理に必要な記録が不足しています"
    } else {
        "原音と位置データから再処理できます"
    };
    Ok(
        json!({"directory":root,"name":root.file_name().unwrap_or_default().to_string_lossy(),"kind":kind,"status":status,"can_reprocess":supported&&stopped&&raw&&logs,"raw_saved":raw,"config":settings(m.get("config").unwrap_or(&json!({})))?,"video_path":m["video_path"].as_str().unwrap_or(""),"message":message,"audio_path":audio_path(&root,false).map(|p|p.to_string_lossy().into_owned()).unwrap_or_default()}),
    )
}
pub fn rename(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty() && name.encode_utf16().count() <= 255,
        "名前は1〜255文字で入力してください"
    );
    ensure!(
        name != "."
            && name != ".."
            && !name.ends_with([' ', '.'])
            && !name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
        "名前に使えない文字、末尾の空白またはピリオドが含まれています"
    );
    let stem = name.split('.').next().unwrap().trim_end().to_uppercase();
    ensure!(
        !["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            && !["COM", "LPT"].iter().any(|prefix| stem
                .strip_prefix(prefix)
                .is_some_and(
                    |n| ["1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"].contains(&n)
                )),
        "この名前はWindowsで予約されています"
    );
    let root = root.canonicalize()?;
    let info = inspect(&root)?;
    ensure!(
        matches!(
            info["status"].as_str(),
            Some("stopped" | "complete" | "failed" | "cancelled")
        ),
        "録音の保存が完了してから名前を変更してください"
    );
    let parent = root.parent().context("録音の保存先が見つかりません")?;
    let destination = parent.join(name);
    ensure!(
        destination.parent() == Some(parent),
        "録音の保存先は変更できません"
    );
    if root.file_name().is_some_and(|current| current == name) {
        return Ok(root);
    }
    ensure!(
        !destination.exists(),
        "同じ名前の録音またはフォルダーが存在します"
    );
    // Keep original manifests and raw bytes as captured; all playback and replay
    // paths are resolved relative to the selected directory, including latest audio.
    fs::rename(&root, &destination)
        .context("名前を変更できません。録音を使用中のアプリやファイルを閉じてください")?;
    Ok(destination)
}
pub fn list(root: &Path) -> Result<Value> {
    let mut rows = Vec::new();
    if !root.is_dir() {
        return Ok(json!(rows));
    }
    for entry in fs::read_dir(root)? {
        let p = entry?.path();
        if p.is_dir() && (p.join("session.json").is_file() || p.join("take.json").is_file()) {
            if rows.len() >= 10000 {
                break;
            }
            rows.push(inspect(&p).unwrap_or_else(|e|json!({"directory":p,"name":p.file_name().unwrap_or_default().to_string_lossy(),"can_reprocess":false,"audio_path":"","message":e.to_string()})));
        }
    }
    rows.sort_by(|a, b| b["name"].as_str().cmp(&a["name"].as_str()));
    Ok(json!(rows))
}
pub struct Lines {
    file: BufReader<File>,
    pub next: Option<Value>,
}
impl Lines {
    pub fn open(path: &Path) -> Result<Self> {
        let mut s = Self {
            file: BufReader::new(File::open(path)?),
            next: None,
        };
        s.advance()?;
        Ok(s)
    }
    pub fn advance(&mut self) -> Result<()> {
        self.next = None;
        loop {
            let mut line = Vec::new();
            loop {
                let bytes = self.file.fill_buf()?;
                if bytes.is_empty() {
                    break;
                }
                let n = bytes
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |i| i + 1);
                ensure!(
                    line.len() + n <= 4 * 1024 * 1024,
                    "Recording log line exceeds 4 MiB"
                );
                let done = bytes[n - 1] == b'\n';
                line.extend(&bytes[..n]);
                self.file.consume(n);
                if done {
                    break;
                }
            }
            if line.is_empty() {
                return Ok(());
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            self.next = Some(serde_json::from_slice(&line)?);
            return Ok(());
        }
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "再処理を中止しました");
    Ok(())
}
fn hash(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        cancelled(cancel)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn event_time(row: &Value) -> Result<i64> {
    row["receive_time_ns"]
        .as_i64()
        .context("Missing OSC receipt time")
}
fn archive(
    output: &mut WaveWriter,
    poses: &mut impl Write,
    blocks: Vec<ProcessedBlock>,
    cancel: &AtomicBool,
) -> Result<()> {
    for block in blocks {
        cancelled(cancel)?;
        output.append(&block.samples)?;
        serde_json::to_writer(&mut *poses, &block.pose)?;
        poses.write_all(b"\n")?;
    }
    Ok(())
}
pub fn reprocess(
    root: &Path,
    options: &Value,
    cancel: &AtomicBool,
    library: &Path,
    mut progress: impl FnMut(Value),
    publish_latest: bool,
) -> Result<Value> {
    let root = root.canonicalize()?;
    let info = inspect(&root)?;
    ensure!(
        info["can_reprocess"].as_bool() == Some(true),
        "{}",
        info["message"].as_str().unwrap_or("Cannot reprocess")
    );
    let m = manifest(&root)?;
    let mut cfg = info["config"].clone();
    for key in ["channel", "gain_db", "source_mode", "mouth_offset_m"] {
        if let Some(v) = options.get(key) {
            cfg[key] = v.clone();
        }
    }
    let cfg = settings(&cfg)?;
    let legacy = m["kind"] == "vr_diagnostic_take";
    let mut inputs = vec![
        "raw/microphone.wav",
        "raw/audio.clock.jsonl",
        if legacy {
            "raw/osc.jsonl"
        } else {
            "raw/osc.events.jsonl"
        },
    ];
    let extra = if legacy {
        "raw/osc.query-state.json"
    } else {
        "raw/pose-bootstrap.json"
    };
    if child(&root, extra)?.is_file() {
        inputs.push(extra);
    }
    let mut hashes = json!({});
    for name in &inputs {
        let digest = hash(&child(&root, name)?, cancel)?;
        if legacy {
            ensure!(
                m["files"][name] == digest,
                "録音のハッシュが一致しません: {name}"
            );
        }
        hashes[name] = json!(digest);
    }
    let mut raw = WaveReader::open(&child(&root, "raw/microphone.wav")?)?;
    let channel = cfg["channel"].as_u64().unwrap() as usize;
    ensure!(
        channel < raw.channels as usize,
        "選択した入力チャンネルは原音にありません"
    );
    let mut clock = Lines::open(&child(&root, "raw/audio.clock.jsonl")?)?;
    let mut events = Lines::open(&child(&root, inputs[2])?)?;
    ensure!(clock.next.is_some(), "録音の時計データがありません");
    let renders = child(&root, "renders")?;
    fs::create_dir_all(&renders)?;
    let folder = renders.join(format!(
        "rust-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros()
    ));
    fs::create_dir(&folder)?;
    let mut report = json!({"kind":"rust_session_reprocess","schema_version":1,"status":"processing","runtime":"rust","config":cfg,"input_hashes":hashes,"python_runtime":false,"project_cpp_runtime":false,"offline_equivalent":false,"output":"binaural.wav","camera_policy":"hold_last_received_pose"});
    save_json(&folder.join("report.json"), &report)?;
    let result = (|| -> Result<()> {
        let mut output = WaveWriter::create(&folder.join("binaural.wav"), 2)?;
        let mut poses = BufWriter::new(File::create(folder.join("applied-poses.jsonl"))?);
        let mut processor = Processor::new(&cfg, 0, library, None)?;
        if let Some(db) = options["gain_db"].as_f64() {
            processor.override_gain(db)?;
        }
        let mut bootstrap = BTreeMap::new();
        let mut serial = 0u64;
        if legacy && hashes.get(extra).is_some() {
            let snapshot = load_json(&child(&root, extra)?)?;
            for (address, row) in snapshot["observations"]
                .as_object()
                .context("Invalid initial observations")?
            {
                if row["http_status"] == 200 {
                    let bytes = STANDARD.decode(
                        row["response_base64"]
                            .as_str()
                            .context("Missing response bytes")?,
                    )?;
                    let event = json!({"kind":"snapshot","receive_time_ns":row["response_end_ns"],"request_start_ns":row["request_start_ns"],"address":address,"body":String::from_utf8(bytes)?});
                    bootstrap.insert((event_time(&event)?, serial), event);
                    serial += 1;
                }
            }
        }
        if !legacy && hashes.get(extra).is_some() {
            let state = load_json(&child(&root, extra)?)?;
            processor.restore(&state)?;
            report["checkpoint_restored"] = json!(true);
            report["checkpoint_bridge_present"] =
                json!(state.get("replay_steps_before_ring").is_some());
        } else {
            report["checkpoint_restored"] = json!(false);
        }
        let mut expected = 0u64;
        let mut packets = 0u64;
        let mut previous_event = -1;
        let mut receipt_reorders = 0u64;
        while let Some(row) = clock.next.take() {
            cancelled(cancel)?;
            let first = row["file_frame_start"]
                .as_u64()
                .context("Missing file frame")?;
            let count = row["frames"]
                .as_u64()
                .context("Missing audio frame count")?;
            ensure!(
                first == expected
                    && count > 0
                    && count <= 480000
                    && first <= raw.frames
                    && count <= raw.frames - first,
                "原音と時計のフレーム対応が不正です"
            );
            let flags: u32 = row["flags"]
                .as_u64()
                .context("Missing packet flags")?
                .try_into()?;
            // Older recorders write null when WASAPI declares its timestamp
            // invalid. Such packets still reset the adapter and preserve raw
            // frame accounting; their timestamp must never become a pose query.
            let at = if flags & 4 != 0 {
                row["recorded_time_ns"].as_i64().unwrap_or(0)
            } else {
                row["recorded_time_ns"]
                    .as_i64()
                    .context("Missing recorded audio time")?
            };
            let until = at
                .checked_add(100_000_000)
                .context("Audio clock overflow")?;
            loop {
                let event_at = events.next.as_ref().map(event_time).transpose()?;
                let boot_at = bootstrap.first_key_value().map(|(key, _)| key.0);
                if !event_at.is_some_and(|t| t <= until) && !boot_at.is_some_and(|t| t <= until) {
                    break;
                }
                if boot_at.is_some_and(|t| t <= until && event_at.is_none_or(|e| t < e)) {
                    processor.event(bootstrap.pop_first().unwrap().1)?;
                } else {
                    let mut event = events.next.take().unwrap();
                    let t = event_time(&event)?;
                    receipt_reorders += u64::from(t < previous_event);
                    previous_event = previous_event.max(t);
                    if legacy {
                        event["kind"] = json!("udp");
                    }
                    processor.event(event)?;
                    events.advance()?;
                }
            }
            let audio = AudioPacket {
                samples: raw.read(first, count as u32)?,
                frames: count as u32,
                channels: raw.channels as u32,
                file: first,
                device: row["device_position_frames"]
                    .as_u64()
                    .context("Missing device position")?,
                flags,
                time: at,
            };
            archive(
                &mut output,
                &mut poses,
                processor.push(&audio, channel)?,
                cancel,
            )?;
            expected += count;
            clock.advance()?;
            packets += 1;
            if packets % 100 == 0 {
                output.flush()?;
                progress(json!({"processed_frames":expected,"total_frames":raw.frames}));
            }
        }
        ensure!(
            expected == raw.frames,
            "原音の末尾に対応する時計データがありません"
        );
        archive(&mut output, &mut poses, processor.finish()?, cancel)?;
        poses.flush()?;
        report["statistics"] = processor.statistics();
        report["receipt_reorders"] = json!(receipt_reorders);
        report["output_frames"] = json!(output.finish()?);
        ensure!(
            report["statistics"]["output_frames"] != report["statistics"]["muted_frames"],
            "カメラと頭部・口元が揃う区間がありません。原音は保持されています"
        );
        for name in &inputs {
            ensure!(
                hash(&child(&root, name)?, cancel)? == hashes[name],
                "再処理中に録音データが変更されました"
            );
        }
        report["status"] = json!("complete");
        save_json(&folder.join("report.json"), &report)?;
        if publish_latest {
            save_json(
                &renders.join("latest-native.json"),
                &json!({"output":folder.join("binaural.wav").strip_prefix(&root)?,"report":folder.join("report.json").strip_prefix(&root)?}),
            )?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        report["status"] = json!(if cancel.load(Ordering::Relaxed) {
            "cancelled"
        } else {
            "failed"
        });
        report["error"] = json!(e.to_string());
        let _ = save_json(&folder.join("report.json"), &report);
        return Err(e);
    }
    report["output_path"] = json!(folder.join("binaural.wav"));
    report["directory"] = json!(folder);
    Ok(report)
}
