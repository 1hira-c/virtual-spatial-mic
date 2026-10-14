mod common;
use anyhow::Result;
use serde_json::json;
use std::fs;
use vsm_core::{session, wave};

#[test]
fn session_video_moves_with_renamed_take() -> Result<()> {
    let base = common::output("video-rename");
    let root = common::fixture(&base)?;
    fs::write(root.join("video.mp4"), b"test video bytes")?;
    let mut manifest = wave::load_json(&root.join("session.json"))?;
    manifest["video_path"] = json!("video.mp4");
    wave::save_json(&root.join("session.json"), &manifest)?;
    let before = wave::sha256(&session::video_path(&root)?)?;
    assert!(
        !session::inspect(&root)?["video_path"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let moved = session::rename(&root, "録画テスト")?;
    assert_eq!(before, wave::sha256(&session::video_path(&moved)?)?);
    assert_eq!(
        session::video_path(&moved)?,
        moved.canonicalize()?.join("video.mp4")
    );
    Ok(())
}

#[test]
fn manifest_cannot_authorize_external_video() -> Result<()> {
    let base = common::output("video-scope");
    let root = common::fixture(&base)?;
    let external = base.join("unrelated.mp4");
    fs::write(&external, b"external")?;
    let mut manifest = wave::load_json(&root.join("session.json"))?;
    for name in [
        external.to_string_lossy().as_ref(),
        "../unrelated.mp4",
        "../../unrelated.mp4",
    ] {
        manifest["video_path"] = json!(name);
        wave::save_json(&root.join("session.json"), &manifest)?;
        assert!(session::video_path(&root).is_err());
        assert_eq!(session::inspect(&root)?["video_path"], "");
    }
    Ok(())
}

#[test]
fn unfinished_video_is_not_offered_for_playback() -> Result<()> {
    let base = common::output("video-pending");
    let root = common::fixture(&base)?;
    let mut manifest = wave::load_json(&root.join("session.json"))?;
    manifest["video_path"] = json!("video.mp4");
    wave::save_json(&root.join("session.json"), &manifest)?;
    fs::write(root.join("video.mp4.pending"), b"pending")?;
    assert!(session::video_path(&root).is_err());
    assert_eq!(session::inspect(&root)?["video_path"], "");
    Ok(())
}
