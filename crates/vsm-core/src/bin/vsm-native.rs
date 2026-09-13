use anyhow::{Context, Result};
use std::{path::Path, sync::atomic::AtomicBool};
use vsm_core::{dsp, session, wave};
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let command=args.get(1).context("Usage: vsm-native list-sessions|inspect-session|reprocess-session FOLDER [OPTIONS.json] [STEAM_AUDIO_LIBRARY]")?;
    let path = Path::new(args.get(2).context("Missing recording path")?);
    let result = match command.as_str() {
        "list-sessions" => session::list(path)?,
        "inspect-session" => session::inspect(path)?,
        "reprocess-session" => {
            let options = if let Some(p) = args.get(3) {
                wave::load_json(Path::new(p))?
            } else {
                serde_json::json!({})
            };
            let lib = args
                .get(4)
                .map(std::path::PathBuf::from)
                .map(Ok)
                .unwrap_or_else(dsp::packaged_library)?;
            let publish = options.get("publish_latest").map_or(Ok(true), |v| {
                v.as_bool().context("publish_latest must be boolean")
            })?;
            session::reprocess(
                path,
                &options,
                &AtomicBool::new(false),
                &lib,
                |_| {},
                publish,
            )?
        }
        _ => anyhow::bail!("Unknown command"),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
