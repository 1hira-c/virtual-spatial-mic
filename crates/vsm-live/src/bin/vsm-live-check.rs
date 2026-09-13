//! Development/hardware verification runner. No Python or project C++ runtime.
use anyhow::{Context, Result};
use serde_json::json;
use std::{path::Path, sync::Arc, thread, time::Duration};
use vsm_core::{dsp, wave};
use vsm_live::{engine::Engine, monitor::Monitor};
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "osc-check") {
        let seconds = args
            .get(2)
            .map(|s| s.parse::<u64>())
            .transpose()?
            .unwrap_or(10)
            .clamp(1, 60);
        let rows = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let count = rows.clone();
        let network = vsm_live::osc::OscLive::new(
            vsm_live::clock::now_ns(),
            Arc::new(move |_| {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }),
        )?;
        println!(
            "{}",
            json!({"pid":std::process::id(),"network":network.status()})
        );
        thread::sleep(Duration::from_secs(seconds));
        let status = network.status();
        println!(
            "{}",
            json!({"network":status,"events":rows.load(std::sync::atomic::Ordering::Relaxed)})
        );
        anyhow::ensure!(
            status["error"].as_str() == Some(""),
            "OSC discovery failed: {status}"
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|a| a == "devices") {
        #[cfg(windows)]
        println!("{}", vsm_live::wasapi::devices()?);
        #[cfg(not(windows))]
        println!("{}", json!({"inputs":[],"outputs":[]}));
        return Ok(());
    }
    let config = wave::load_json(Path::new(
        args.get(1).context("Expected config JSON or devices")?,
    ))?;
    let library = args
        .get(2)
        .map(std::path::PathBuf::from)
        .unwrap_or(dsp::packaged_library()?);
    let monitor = if let Some(id) = config["monitor_endpoint"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        Some(Arc::new(Monitor::new(id.into())?))
    } else {
        None
    };
    let out = monitor.clone();
    let mut engine = Engine::start(
        config.clone(),
        library,
        Arc::new(move |samples, at| {
            if let Some(m) = &out {
                m.push(samples, at)?;
            }
            Ok(())
        }),
    )?;
    thread::sleep(Duration::from_secs(2));
    engine.command(json!({"event":"recording_started","source":"hardware_check"}))?;
    thread::sleep(Duration::from_secs(3));
    engine.command(json!({"event":"recording_stopped"}))?;
    thread::sleep(Duration::from_secs(1));
    engine.join();
    let report = json!({"engine":engine.status(),"monitor":monitor.as_ref().map(|m|m.status())});
    if let Some(path) = config["check_report_path"].as_str() {
        wave::save_json(Path::new(path), &report)?;
    }
    println!("{report}");
    anyhow::ensure!(report["engine"]["state"] == "stopped", "Live engine failed");
    Ok(())
}
