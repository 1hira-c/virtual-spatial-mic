#![allow(unsafe_op_in_unsafe_fn, non_upper_case_globals)]
use crate::{api::*, bindings::*};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use vsm_live::engine::Engine;

struct Control {
    config: Value,
    engine: Option<Engine>,
    armed: bool,
    active: bool,
    recording: bool,
    closing: bool,
    registered: bool,
}
struct Live {
    source: SourcePtr,
    control: Mutex<Control>,
    info: Arc<Mutex<Value>>,
    observer_stop: Arc<AtomicBool>,
    observer: Mutex<Option<JoinHandle<()>>>,
}
impl Live {
    fn stop(&self, c: &mut Control) {
        self.observer_stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.observer.lock().unwrap().take() {
            let _ = w.join();
        }
        if let Some(mut engine) = c.engine.take() {
            engine.join();
            *self.info.lock().unwrap() = engine.status();
        }
        c.recording = false;
    }
    fn start(&self, c: &mut Control) -> Result<()> {
        if c.closing || c.engine.as_ref().is_some_and(|e| !e.finished()) {
            return Ok(());
        }
        if c.config["fixture"] != true && c.config["endpoint_id"] == "" {
            return Ok(());
        }
        self.stop(c);
        let source = self.source;
        let engine = Engine::start(
            c.config.clone(),
            library()?,
            Arc::new(move |samples, at| source.output(samples, at)),
        )?;
        let status = engine.shared_status();
        let info = self.info.clone();
        let stop = self.observer_stop.clone();
        stop.store(false, Ordering::Relaxed);
        *self.observer.lock().unwrap() = Some(thread::spawn(move || {
            let mut previous = String::new();
            while !stop.load(Ordering::Relaxed) {
                let state = status.lock().unwrap().clone();
                *info.lock().unwrap() = state.clone();
                let key = format!(
                    "{}{}{}{}",
                    state["state"],
                    state["message"],
                    state["archive_state"],
                    state["archive_error"]
                );
                if key != previous {
                    previous = key;
                    source.changed();
                    if state["state"] == "failed" || state["archive_state"] == "failed" {
                        log(&state.to_string());
                    }
                }
                thread::sleep(Duration::from_millis(250));
            }
        }));
        c.engine = Some(engine);
        Ok(())
    }
    fn event(&self, c: &mut Control, event: &str, joined: bool) -> Result<()> {
        if let Some(e) = &c.engine {
            let mut row = json!({"event":event,"source":unsafe{string((api().obs_source_get_name)(self.source.0))},"joined_recording":joined});
            if event == "recording_stopped" {
                if let Some(f) = &api().frontend {
                    unsafe {
                        let path = (f.last)();
                        row["video_path"] = json!(string(path));
                        if !path.is_null() {
                            (api().bfree)(path.cast());
                        }
                    }
                }
            }
            e.command(row)?;
        }
        Ok(())
    }
    fn join_recording(&self, c: &mut Control) -> Result<()> {
        if let Some(f) = &api().frontend {
            if unsafe { (f.recording)() }
                && c.engine.as_ref().is_some_and(|e| !e.finished())
                && !c.recording
            {
                c.recording = true;
                self.event(c, "recording_started", true)?;
            }
        }
        Ok(())
    }
    unsafe fn update(&self, data: *mut obs_data_t) -> Result<()> {
        let a = api();
        let mut config = json!({"save_raw":(a.obs_data_get_bool)(data,c"save_raw".as_ptr()),"endpoint_id":setting(data,c"endpoint_id"),"output_root":setting(data,c"output_root"),"gain_db":(a.obs_data_get_double)(data,c"gain_db".as_ptr()),"channel":(a.obs_data_get_int)(data,c"channel".as_ptr()),"mouth_offset_m":[(a.obs_data_get_double)(data,c"mouth_x".as_ptr()),(a.obs_data_get_double)(data,c"mouth_y".as_ptr()),(a.obs_data_get_double)(data,c"mouth_z".as_ptr())]});
        let fixture = setting(data, c"fixture_config");
        if !fixture.is_empty() {
            let f = vsm_core::wave::load_json(Path::new(&fixture))?;
            config.as_object_mut().unwrap().extend(
                f.as_object()
                    .ok_or_else(|| anyhow::anyhow!("Invalid fixture config"))?
                    .clone(),
            );
        }
        let armed = (a.obs_data_get_bool)(data, c"input_enabled".as_ptr());
        vsm_core::session::settings(&config)?;
        let mut c = self.control.lock().unwrap();
        if c.config != config || c.armed != armed {
            self.stop(&mut c);
            c.config = config;
            c.armed = armed;
        }
        if api().frontend.is_some() && c.armed && (c.active || (a.obs_source_active)(self.source.0))
        {
            self.start(&mut c)?;
            self.join_recording(&mut c)?;
        }
        Ok(())
    }
    fn error(&self, e: impl std::fmt::Display) {
        let mut info = self.info.lock().unwrap();
        info["message"] = json!(e.to_string());
        info["state"] = json!("failed");
        log(&format!("Live source: {e}"));
    }
    fn last(&self) -> Option<PathBuf> {
        self.info.lock().unwrap()["last_directory"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        let c = self.control.get_mut().unwrap();
        c.closing = true;
        if c.registered {
            if let Some(f) = &api().frontend {
                unsafe {
                    (f.remove)(on_event, self as *mut Self as *mut c_void);
                }
            }
        }
        let mut c = self.control.lock().unwrap();
        self.stop(&mut c);
    }
}
unsafe extern "C" fn on_event(event: obs_frontend_event, data: *mut c_void) {
    guard(|| {
        let s = &*(data as *mut Live);
        let result = (|| -> Result<()> {
            let mut c = s.control.lock().unwrap();
            if c.closing {
                return Ok(());
            }
            match event {
                obs_frontend_event_OBS_FRONTEND_EVENT_EXIT
                | obs_frontend_event_OBS_FRONTEND_EVENT_SCENE_COLLECTION_CLEANUP => {
                    c.closing = event == obs_frontend_event_OBS_FRONTEND_EVENT_EXIT;
                    s.stop(&mut c);
                }
                obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED => {
                    if c.armed && (api().obs_source_active)(s.source.0) {
                        s.start(&mut c)?;
                        if !c.recording {
                            c.recording = true;
                            s.event(&mut c, "recording_started", false)?;
                        }
                    }
                }
                obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STOPPED => {
                    if c.recording {
                        c.recording = false;
                        s.event(&mut c, "recording_stopped", false)?;
                    }
                }
                obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_PAUSED
                | obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_UNPAUSED
                | obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STOPPING => {
                    if c.recording {
                        s.event(
                            &mut c,
                            match event {
                                obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_PAUSED => {
                                    "recording_paused"
                                }
                                obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_UNPAUSED => {
                                    "recording_unpaused"
                                }
                                _ => "recording_stopping",
                            },
                            false,
                        )?;
                    }
                }
                obs_frontend_event_OBS_FRONTEND_EVENT_FINISHED_LOADING => {
                    if c.armed && (api().obs_source_active)(s.source.0) {
                        c.active = true;
                        s.start(&mut c)?;
                        s.join_recording(&mut c)?;
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = result {
            s.error(e);
        }
    })
}
unsafe extern "C" fn defaults(d: *mut obs_data_t) {
    guard(|| {
        let a = api();
        (a.obs_data_set_default_bool)(d, c"save_raw".as_ptr(), true);
        (a.obs_data_set_default_bool)(d, c"input_enabled".as_ptr(), true);
        (a.obs_data_set_default_double)(d, c"gain_db".as_ptr(), -18.);
        (a.obs_data_set_default_int)(d, c"channel".as_ptr(), 0);
        for (k, v) in [
            (c"mouth_x", 0.),
            (c"mouth_y", 0.0064),
            (c"mouth_z", -0.0736),
        ] {
            (a.obs_data_set_default_double)(d, k.as_ptr(), v);
        }
    })
}
unsafe extern "C" fn create(d: *mut obs_data_t, source: *mut obs_source_t) -> *mut c_void {
    guard(|| {
        let s = Box::new(Live {
            source: SourcePtr(source),
            control: Mutex::new(Control {
                config: json!({}),
                engine: None,
                armed: false,
                active: false,
                recording: false,
                closing: false,
                registered: false,
            }),
            info: Arc::new(Mutex::new(json!({"message":"入力待機中"}))),
            observer_stop: Arc::new(AtomicBool::new(false)),
            observer: Mutex::new(None),
        });
        if let Err(e) = s.update(d) {
            log(&e.to_string());
            return std::ptr::null_mut();
        }
        let p = Box::into_raw(s);
        if let Some(f) = &api().frontend {
            (f.add)(on_event, p.cast());
            (*p).control.lock().unwrap().registered = true;
        }
        p.cast()
    })
}
unsafe extern "C" fn destroy(p: *mut c_void) {
    guard(|| drop(Box::from_raw(p as *mut Live)))
}
unsafe extern "C" fn update(p: *mut c_void, d: *mut obs_data_t) {
    guard(|| {
        let s = &*(p as *mut Live);
        if let Err(e) = s.update(d) {
            s.error(e);
        }
    })
}
unsafe extern "C" fn activate(p: *mut c_void) {
    guard(|| {
        let s = &*(p as *mut Live);
        let mut c = s.control.lock().unwrap();
        c.active = true;
        if c.armed && api().frontend.is_some() {
            if let Err(e) = s.start(&mut c).and_then(|_| s.join_recording(&mut c)) {
                s.error(e);
            }
        }
    })
}
unsafe extern "C" fn deactivate(p: *mut c_void) {
    guard(|| {
        let s = &*(p as *mut Live);
        let mut c = s.control.lock().unwrap();
        c.active = false;
        if c.recording {
            if let Err(e) = s.event(&mut c, "source_inactive", false) {
                s.error(e);
            }
        }
    })
}
unsafe extern "C" fn start_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if p.is_null() {
            return false;
        }
        let s = &*(p as *mut Live);
        let mut c = s.control.lock().unwrap();
        if let Err(e) = s.start(&mut c).and_then(|_| {
            if api().frontend.is_none() {
                c.recording = true;
                s.event(&mut c, "recording_started", false)
            } else {
                s.join_recording(&mut c)
            }
        }) {
            s.error(e);
        }
        true
    })
}
unsafe extern "C" fn stop_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if p.is_null() {
            return false;
        }
        let s = &*(p as *mut Live);
        s.stop(&mut s.control.lock().unwrap());
        true
    })
}
unsafe extern "C" fn folder_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if !p.is_null() {
            let s = &*(p as *mut Live);
            if let Some(dir) = s.last() {
                if let Err(e) = open::that_detached(dir) {
                    s.error(e);
                }
            }
        }
        false
    })
}
unsafe extern "C" fn listen_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if !p.is_null() {
            let s = &*(p as *mut Live);
            if let Some(dir) = s.last() {
                match vsm_core::session::audio_path(&dir, false)
                    .and_then(|p| Ok(open::that_detached(p)?))
                {
                    Ok(()) => {}
                    Err(e) => s.error(e),
                }
            }
        }
        false
    })
}
unsafe extern "C" fn properties(p: *mut c_void) -> *mut obs_properties_t {
    guard(|| {
        let a = api();
        let props = (a.obs_properties_create)();
        let s = if p.is_null() {
            None
        } else {
            Some(&*(p as *mut Live))
        };
        text(
            props,
            c"about",
            "マイクと位置を継続して受信し、立体音声をOBSへ送ります。\n待機中は直近10秒をメモリ内に保持。OBS録画の開始・停止で1本ずつ保存します。\nモニターはOBSの「オーディオの詳細プロパティ」で有効にできます。",
        );
        if a.frontend.is_some() {
            (a.obs_properties_add_bool)(
                props,
                c"input_enabled".as_ptr(),
                c"マイク入力・モニターを有効にする".as_ptr(),
            );
        }
        let info = s
            .map(|s| s.info.lock().unwrap().clone())
            .unwrap_or(json!({}));
        text(
            props,
            c"status",
            &format!(
                "{}\n{}",
                info["message"].as_str().unwrap_or("入力待機中"),
                info["archive_error"].as_str().unwrap_or("")
            ),
        );
        let mic = (a.obs_properties_add_list)(
            props,
            c"endpoint_id".as_ptr(),
            c"マイク".as_ptr(),
            obs_combo_type_OBS_COMBO_TYPE_LIST,
            obs_combo_format_OBS_COMBO_FORMAT_STRING,
        );
        (a.obs_property_list_add_string)(mic, c"マイクを選んでください".as_ptr(), c"".as_ptr());
        if let Ok(Ok(devices)) = thread::spawn(vsm_live::wasapi::devices).join() {
            for d in devices["inputs"].as_array().unwrap() {
                (a.obs_property_list_add_string)(
                    mic,
                    c(d["name"].as_str().unwrap_or("")).as_ptr(),
                    c(d["endpoint_id"].as_str().unwrap_or("")).as_ptr(),
                );
            }
        }
        let channel = (a.obs_properties_add_list)(
            props,
            c"channel".as_ptr(),
            c"使用チャンネル".as_ptr(),
            obs_combo_type_OBS_COMBO_TYPE_LIST,
            obs_combo_format_OBS_COMBO_FORMAT_INT,
        );
        (a.obs_property_list_add_int)(channel, c"1（左 / モノラル）".as_ptr(), 0);
        (a.obs_property_list_add_int)(channel, c"2（右）".as_ptr(), 1);
        (a.obs_properties_add_path)(
            props,
            c"output_root".as_ptr(),
            c"音声・動きの保存先".as_ptr(),
            obs_path_type_OBS_PATH_DIRECTORY,
            std::ptr::null(),
            std::ptr::null(),
        );
        (a.obs_properties_add_float_slider)(
            props,
            c"gain_db".as_ptr(),
            c"入力レベル (dB)".as_ptr(),
            -60.,
            0.,
            1.,
        );
        (a.obs_properties_add_bool)(
            props,
            c"save_raw".as_ptr(),
            c"原音・位置データを保存（後から再調整するため）".as_ptr(),
        );
        let (running, recording) = s
            .map(|s| {
                let c = s.control.lock().unwrap();
                (
                    c.engine.as_ref().is_some_and(|e| !e.finished()),
                    c.recording,
                )
            })
            .unwrap_or((false, false));
        if a.frontend.is_none() {
            button(
                props,
                c"live_start",
                "ライブ録音を開始",
                Some(start_button),
                p,
            );
            button(props, c"live_stop", "停止して保存", Some(stop_button), p);
            enabled(props, c"live_start", !running);
            enabled(props, c"live_stop", running);
        } else {
            button(props, c"reconnect", "入力を再接続", Some(start_button), p);
            enabled(props, c"reconnect", !running);
        }
        button(
            props,
            c"live_listen",
            "前回の処理音声を聴く",
            Some(listen_button),
            p,
        );
        button(
            props,
            c"live_folder",
            "前回の保存フォルダーを開く",
            Some(folder_button),
            p,
        );
        enabled(
            props,
            c"live_listen",
            s.and_then(Live::last)
                .is_some_and(|p| p.join("live-output.wav").is_file()),
        );
        for k in [
            c"endpoint_id",
            c"channel",
            c"output_root",
            c"gain_db",
            c"save_raw",
        ] {
            enabled(
                props,
                k,
                if a.frontend.is_some() {
                    !recording
                } else {
                    !running
                },
            );
        }
        props
    })
}
unsafe extern "C" fn name(_: *mut c_void) -> *const std::ffi::c_char {
    c"Virtual Spatial Mic（ライブ録音）".as_ptr()
}
pub unsafe fn register() {
    let info = obs_source_info {
        id: c"vbs_live".as_ptr(),
        type_: obs_source_type_OBS_SOURCE_TYPE_INPUT,
        output_flags: OBS_SOURCE_AUDIO,
        get_name: Some(name),
        create: Some(create),
        destroy: Some(destroy),
        get_defaults: Some(defaults),
        get_properties: Some(properties),
        update: Some(update),
        activate: Some(activate),
        deactivate: Some(deactivate),
        ..Default::default()
    };
    (api().obs_register_source_s)(&info, std::mem::size_of_val(&info));
}
