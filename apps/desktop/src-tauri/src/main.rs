#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
use tauri::{Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use vsm_core::{session, wave};
use vsm_live::{engine::Engine, monitor::Monitor};

struct Render {
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<Value>>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for Render {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
struct Studio {
    config: Value,
    settings_path: PathBuf,
    library: PathBuf,
    fixture: Value,
    engine: Option<Engine>,
    monitor: Arc<Mutex<Option<Monitor>>>,
    render: Option<Render>,
    sessions: HashSet<PathBuf>,
    output_roots: HashSet<PathBuf>,
}
impl Studio {
    fn authorize(&self, path: &str) -> Result<PathBuf> {
        let p = Path::new(path).canonicalize()?;
        ensure!(
            self.sessions.contains(&p),
            "一覧またはフォルダー選択から録音を選んでください"
        );
        Ok(p)
    }
    fn running(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| !e.finished())
    }
    fn rendering(&self) -> bool {
        self.render
            .as_ref()
            .is_some_and(|r| r.worker.as_ref().is_some_and(|w| !w.is_finished()))
    }
    fn close(&mut self) {
        self.render.take();
        self.engine.take();
        self.monitor.lock().unwrap().take();
    }
}
impl Drop for Studio {
    fn drop(&mut self) {
        self.close();
    }
}
type Shared = Mutex<Studio>;
type CommandResult<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn plain(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(t) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{t}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(&s).into()
    }
}

#[tauri::command]
fn studio_status(state: State<Shared>) -> CommandResult<Value> {
    let s = state.lock().map_err(err)?;
    let monitor = s.monitor.lock().map_err(err)?;
    Ok(
        json!({"config":s.config,"fixture":s.fixture.is_object(),"engine":s.engine.as_ref().map(Engine::status).unwrap_or(json!({"active":false,"state":"stopped","message":"入力は無効です"})),"monitor":monitor.as_ref().map(Monitor::status).unwrap_or(json!({"ready":false})),"monitor_enabled":monitor.is_some(),"render":s.render.as_ref().map(|r|r.state.lock().unwrap().clone()).unwrap_or(json!({"active":false})),"runtime":"Rust / Tauri"}),
    )
}

#[tauri::command]
fn audio_levels(state: State<Shared>) -> CommandResult<Value> {
    let s = state.lock().map_err(err)?;
    Ok(
        json!({"active":s.running(),"peaks":s.engine.as_ref().map(Engine::audio_levels).unwrap_or([0.;3])}),
    )
}

#[tauri::command]
async fn audio_devices() -> CommandResult<Value> {
    tauri::async_runtime::spawn_blocking(||{
    #[cfg(windows)] { vsm_live::wasapi::devices().map_err(err) }
    #[cfg(not(windows))] { Ok(json!({"inputs":[],"outputs":[],"message":"このOSのライブ入力・出力は未対応です。保存済み録音を再処理できます。"})) }
}).await.map_err(err)?
}

#[tauri::command]
fn configure_gain(gain_db: f64, state: State<Shared>) -> CommandResult<f64> {
    let mut s = state.lock().map_err(err)?;
    let mut config = s.config.clone();
    config["gain_db"] = json!(gain_db);
    let config = session::settings(&config).map_err(err)?;
    if s.running() {
        s.engine.as_ref().unwrap().set_gain(gain_db).map_err(err)?;
    }
    s.config = config;
    session::save_settings(&s.settings_path, &s.config).map_err(err)?;
    Ok(gain_db)
}

#[tauri::command]
fn configure(value: Value, state: State<Shared>) -> CommandResult<Value> {
    let mut s = state.lock().map_err(err)?;
    ensure_configurable(&s).map_err(err)?;
    let value = session::settings(&value).map_err(err)?;
    let root = PathBuf::from(value["output_root"].as_str().unwrap());
    if !s.output_roots.contains(&root) {
        return Err("保存先はフォルダー選択で指定してください".into());
    }
    session::save_settings(&s.settings_path, &value).map_err(err)?;
    s.config = value.clone();
    Ok(value)
}
fn ensure_configurable(s: &Studio) -> Result<()> {
    ensure!(
        !s.running(),
        "入力を無効にしてから録音設定を変更してください"
    );
    Ok(())
}

#[tauri::command]
async fn choose_folder(
    purpose: String,
    app: tauri::AppHandle,
    state: State<'_, Shared>,
) -> CommandResult<Option<Value>> {
    if purpose != "recordings" && purpose != "session" {
        return Err("Invalid folder purpose".into());
    }
    if purpose == "recordings" {
        ensure_configurable(&*state.lock().map_err(err)?).map_err(err)?;
    }
    let picking_session = purpose == "session";
    let picker = app.clone();
    let path = tauri::async_runtime::spawn_blocking(move || {
        picker
            .dialog()
            .file()
            .set_title(if picking_session {
                "録音フォルダーを開く"
            } else {
                "録音の保存先"
            })
            .blocking_pick_folder()
            .map(|p| p.into_path())
    })
    .await
    .map_err(err)?;
    let Some(path) = path else { return Ok(None) };
    let path = path.map_err(err)?;
    // A recording folder has a manifest; a storage root contains recording folders.
    let is_session = path.join("session.json").is_file() || path.join("take.json").is_file();
    let mut s = state.lock().map_err(err)?;
    if picking_session {
        if !is_session {
            return Err(
                "session.json または take.json がある録音フォルダーを選んでください".into(),
            );
        }
        let info = session::inspect(&path).map_err(err)?;
        s.sessions.insert(path.canonicalize().map_err(err)?);
        Ok(Some(json!({"session":info})))
    } else {
        ensure_configurable(&s).map_err(err)?;
        s.output_roots.insert(PathBuf::from(plain(&path)));
        s.config["output_root"] = json!(plain(&path));
        session::save_settings(&s.settings_path, &s.config).map_err(err)?;
        Ok(Some(json!({"config":s.config})))
    }
}

#[tauri::command]
fn recordings(state: State<Shared>) -> CommandResult<Value> {
    let mut s = state.lock().map_err(err)?;
    let rows = session::list(Path::new(s.config["output_root"].as_str().unwrap())).map_err(err)?;
    for row in rows.as_array().unwrap() {
        if let Some(path) = row["directory"].as_str() {
            if let Ok(p) = Path::new(path).canonicalize() {
                s.sessions.insert(p);
            }
        }
    }
    Ok(rows)
}

#[tauri::command]
fn input_enable(enabled: bool, state: State<Shared>) -> CommandResult<Value> {
    let mut s = state.lock().map_err(err)?;
    if enabled {
        ensure_configurable(&s).map_err(err)?;
        s.engine.take();
        let mut cfg = s.config.clone();
        if let Some(f) = s.fixture.as_object() {
            cfg.as_object_mut().unwrap().extend(f.clone());
        } else if cfg["endpoint_id"] == "" {
            return Err("マイクを選んでください".into());
        }
        let output = s.monitor.clone();
        s.engine = Some(
            Engine::start(
                cfg,
                s.library.clone(),
                Arc::new(move |samples, at| {
                    if let Some(m) = output.lock().unwrap().as_ref() {
                        m.push(samples, at)?;
                    }
                    Ok(())
                }),
            )
            .map_err(err)?,
        );
    } else {
        s.engine.take();
        s.monitor.lock().map_err(err)?.take();
    }
    Ok(json!({"enabled":enabled}))
}

#[tauri::command]
fn record_control(recording: bool, state: State<Shared>) -> CommandResult<()> {
    let s = state.lock().map_err(err)?;
    let engine = s.engine.as_ref().ok_or("先に入力を有効にしてください")?;
    if !s.running() {
        return Err("入力が停止しています".into());
    }
    engine.command(json!({"event":if recording{"recording_started"}else{"recording_stopped"},"source":"studio"})).map_err(err)
}

#[tauri::command]
fn monitor_enable(enabled: bool, endpoint: String, state: State<Shared>) -> CommandResult<()> {
    let mut s = state.lock().map_err(err)?;
    if enabled && !s.running() {
        return Err("先に入力を有効にしてください".into());
    }
    let monitor = if enabled {
        Some(Monitor::new(endpoint.clone()).map_err(err)?)
    } else {
        None
    };
    *s.monitor.lock().map_err(err)? = monitor;
    s.config["monitor_endpoint"] = json!(endpoint);
    session::save_settings(&s.settings_path, &s.config).map_err(err)
}

#[tauri::command]
fn reprocess_recording(path: String, options: Value, state: State<Shared>) -> CommandResult<()> {
    let mut s = state.lock().map_err(err)?;
    if s.rendering() {
        return Err("再処理が進行中です".into());
    }
    let path = s.authorize(&path).map_err(err)?;
    s.render.take();
    let library = s.library.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(
        json!({"active":true,"progress":0.,"message":"原音を確認しています"}),
    ));
    let stop = cancel.clone();
    let status = progress.clone();
    let worker=thread::Builder::new().name("vsm-reprocess".into()).spawn(move||{
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||session::reprocess(&path,&options,&stop,&library,|p|{*status.lock().unwrap()=json!({"active":true,"progress":p["processed_frames"].as_f64().unwrap_or(0.)/p["total_frames"].as_f64().unwrap_or(1.).max(1.),"message":"再処理しています"});},true)));
    *status.lock().unwrap()=match result{Ok(Ok(report))=>json!({"active":false,"progress":1.,"message":"再処理が完了しました","report":report}),Ok(Err(e))=>json!({"active":false,"error":e.to_string(),"cancelled":stop.load(Ordering::Relaxed)}),Err(_)=>json!({"active":false,"error":"再処理スレッドが異常終了しました"})};
}).map_err(err)?;
    s.render = Some(Render {
        cancel,
        state: progress,
        worker: Some(worker),
    });
    Ok(())
}

#[tauri::command]
fn cancel_reprocess(state: State<Shared>) -> CommandResult<()> {
    if let Some(r) = &state.lock().map_err(err)?.render {
        r.cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
fn rename_recording(path: String, name: String, state: State<Shared>) -> CommandResult<Value> {
    let mut s = state.lock().map_err(err)?;
    if s.rendering() {
        return Err("再処理が完了してから名前を変更してください".into());
    }
    let root = s.authorize(&path).map_err(err)?;
    let destination = session::rename(&root, &name).map_err(err)?;
    s.sessions.remove(&root);
    s.sessions.insert(destination.clone());
    session::inspect(&destination).map_err(err)
}

#[tauri::command]
fn recording_audio(
    path: String,
    original: bool,
    app: tauri::AppHandle,
    state: State<Shared>,
) -> CommandResult<String> {
    let root = state.lock().map_err(err)?.authorize(&path).map_err(err)?;
    let audio = session::audio_path(&root, original).map_err(err)?;
    app.asset_protocol_scope().allow_file(&audio).map_err(err)?;
    Ok(plain(&audio))
}

#[tauri::command]
fn reveal_recordings_root(app: tauri::AppHandle, state: State<Shared>) -> CommandResult<()> {
    // Use the configured storage root, never a path supplied by the WebView or
    // the selected take. Opening an empty library should work before recording.
    let root = {
        let s = state.lock().map_err(err)?;
        PathBuf::from(
            s.config["output_root"]
                .as_str()
                .ok_or("保存先が未設定です")?,
        )
    };
    std::fs::create_dir_all(&root).map_err(err)?;
    let root = root.canonicalize().map_err(err)?;
    app.opener()
        .open_path(plain(&root), None::<&str>)
        .map_err(err)
}

#[tauri::command]
fn reveal_recording(
    path: String,
    app: tauri::AppHandle,
    state: State<Shared>,
) -> CommandResult<()> {
    let root = state.lock().map_err(err)?.authorize(&path).map_err(err)?;
    app.opener()
        .open_path(plain(&root), None::<&str>)
        .map_err(err)
}

fn run() -> Result<()> {
    let mut fixture = Value::Null;
    let mut settings_override = None;
    let mut library = vsm_core::dsp::packaged_library()?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture-config" => {
                fixture = wave::load_json(Path::new(&args.next().context("Missing fixture path")?))?
            }
            "--settings" => {
                settings_override =
                    Some(PathBuf::from(args.next().context("Missing settings path")?))
            }
            "--steam-audio" => {
                library = PathBuf::from(args.next().context("Missing SDK library path")?)
            }
            _ => anyhow::bail!("Unknown option: {arg}"),
        }
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let settings_path = settings_override
                .clone()
                .unwrap_or(app.path().app_local_data_dir()?.join("settings.json"));
            let mut config = session::load_settings(&settings_path)?;
            if config["output_root"] == "" {
                config["output_root"] = json!(plain(
                    &app.path()
                        .document_dir()?
                        .join("Virtual Spatial Mic/Recordings")
                ));
            }
            if let Some(root) = fixture["output_root"].as_str() {
                config["output_root"] = json!(root);
            }
            let output_roots =
                HashSet::from([PathBuf::from(config["output_root"].as_str().unwrap())]);
            app.manage(Mutex::new(Studio {
                config,
                settings_path,
                library: library.clone(),
                fixture: fixture.clone(),
                engine: None,
                monitor: Arc::new(Mutex::new(None)),
                render: None,
                sessions: HashSet::new(),
                output_roots,
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            studio_status,
            audio_levels,
            audio_devices,
            configure,
            configure_gain,
            choose_folder,
            recordings,
            input_enable,
            record_control,
            monitor_enable,
            reprocess_recording,
            cancel_reprocess,
            recording_audio,
            rename_recording,
            reveal_recordings_root,
            reveal_recording
        ])
        .build(tauri::generate_context!())?
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<Shared>() {
                    state.lock().unwrap().close();
                }
            }
        });
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("Virtual Spatial Mic: {e:#}");
    }
}
