mod common;
use anyhow::Result;
use serde_json::json;
use std::{fs, sync::atomic::AtomicBool};
use vsm_core::{session, wave};

#[test]
fn rename_preserves_originals_and_replay() -> Result<()> {
    let base = common::output("rename");
    let root = common::fixture(&base)?;
    let raw_hash = wave::sha256(&root.join("raw/microphone.wav"))?;
    let manifest_hash = wave::sha256(&root.join("session.json"))?;
    let cancel = AtomicBool::new(false);
    session::reprocess(&root, &json!({}), &cancel, &common::library(), |_| {}, true)?;
    let output_hash = wave::sha256(&session::audio_path(&root, false)?)?;
    let occupied = base.join("既存の録音");
    fs::create_dir(&occupied)?;
    fs::write(occupied.join("keep.txt"), "preserve")?;
    for invalid in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "C:escape",
        "a:b",
        "bad?",
        "end.",
        "end ",
        "NUL",
        "con.wav",
        "LPT1",
        "COM¹",
        "既存の録音",
    ] {
        assert!(session::rename(&root, invalid).is_err(), "{invalid}");
    }
    assert_eq!(fs::read_to_string(occupied.join("keep.txt"))?, "preserve");
    let moved = session::rename(&root, "口元テスト その1")?;
    assert!(!root.exists());
    assert_eq!(session::inspect(&moved)?["name"], "口元テスト その1");
    assert_eq!(raw_hash, wave::sha256(&session::audio_path(&moved, true)?)?);
    assert_eq!(manifest_hash, wave::sha256(&moved.join("session.json"))?);
    assert_eq!(
        output_hash,
        wave::sha256(&session::audio_path(&moved, false)?)?
    );
    session::reprocess(
        &moved,
        &json!({}),
        &cancel,
        &common::library(),
        |_| {},
        true,
    )?;
    assert_eq!(
        output_hash,
        wave::sha256(&session::audio_path(&moved, false)?)?
    );
    let mut manifest = wave::load_json(&moved.join("session.json"))?;
    manifest["status"] = json!("recording");
    wave::save_json(&moved.join("session.json"), &manifest)?;
    assert!(session::rename(&moved, "録音中は変更不可").is_err());
    assert!(moved.exists());
    Ok(())
}
