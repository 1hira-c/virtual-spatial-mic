#![allow(unsafe_op_in_unsafe_fn)]
use crate::{api::*, bindings::*};
use anyhow::{Result, ensure};

use std::{
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use vsm_core::{dsp::Level, playback::RecordingStream};
use vsm_live::clock;
struct Shared {
    cancel: AtomicBool,
    paused: AtomicBool,
    cursor: AtomicU64,
    duration: AtomicU64,
    gain: AtomicU64,
    state: AtomicI32,
    error: Mutex<String>,
}
struct Control {
    path: PathBuf,
    worker: Option<JoinHandle<()>>,
}
struct Replay {
    source: SourcePtr,
    control: Mutex<Control>,
    state: Arc<Shared>,
}
impl Replay {
    fn stop_locked(&self, c: &mut Control) {
        self.state.cancel.store(true, Ordering::Relaxed);
        if let Some(w) = c.worker.take() {
            let _ = w.join();
        }
        self.state.paused.store(false, Ordering::Relaxed);
        self.state
            .state
            .store(obs_media_state_OBS_MEDIA_STATE_STOPPED, Ordering::Relaxed);
        self.state.cursor.store(0, Ordering::Relaxed);
    }
    fn stop(&self) {
        self.stop_locked(&mut self.control.lock().unwrap());
    }
    fn start(&self) {
        let mut c = self.control.lock().unwrap();
        self.stop_locked(&mut c);
        self.state.cancel.store(false, Ordering::Relaxed);
        self.state.duration.store(0, Ordering::Relaxed);
        self.state.error.lock().unwrap().clear();
        self.state
            .state
            .store(obs_media_state_OBS_MEDIA_STATE_OPENING, Ordering::Relaxed);
        let source = self.source;
        let state = self.state.clone();
        let folder = c.path.clone();
        c.worker = Some(thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(source, &state, &folder)
            }));
            if !state.cancel.load(Ordering::Relaxed) {
                match result {
                    Ok(Ok(())) => {
                        state
                            .state
                            .store(obs_media_state_OBS_MEDIA_STATE_ENDED, Ordering::Relaxed);
                        unsafe {
                            (api().obs_source_media_ended)(source.0);
                        }
                    }
                    other => {
                        let message = match other {
                            Ok(Err(e)) => e.to_string(),
                            _ => "再生スレッドが異常終了しました".into(),
                        };
                        state
                            .state
                            .store(obs_media_state_OBS_MEDIA_STATE_ERROR, Ordering::Relaxed);
                        *state.error.lock().unwrap() = message.clone();
                        log(&message);
                        source.changed();
                    }
                }
            }
        }));
    }
    fn pause(&self, pause: bool) {
        if !pause
            && ![
                obs_media_state_OBS_MEDIA_STATE_PLAYING,
                obs_media_state_OBS_MEDIA_STATE_PAUSED,
                obs_media_state_OBS_MEDIA_STATE_OPENING,
            ]
            .contains(&self.state.state.load(Ordering::Relaxed))
        {
            self.start();
            return;
        }
        self.state.paused.store(pause, Ordering::Relaxed);
        self.state.state.store(
            if pause {
                obs_media_state_OBS_MEDIA_STATE_PAUSED
            } else {
                obs_media_state_OBS_MEDIA_STATE_PLAYING
            },
            Ordering::Relaxed,
        );
    }
    unsafe fn update(&self, data: *mut obs_data_t) {
        let db = (api().obs_data_get_double)(data, c"gain_db".as_ptr());
        self.state.gain.store(
            if db.is_finite() {
                10f64.powf(db.clamp(-60., 0.) / 20.)
            } else {
                0.
            }
            .to_bits(),
            Ordering::Relaxed,
        );
        let path = PathBuf::from(setting(data, c"recording"));
        let mut c = self.control.lock().unwrap();
        if c.path != path {
            self.stop_locked(&mut c);
            c.path = path;
            self.state.duration.store(0, Ordering::Relaxed);
        }
    }
}
impl Drop for Replay {
    fn drop(&mut self) {
        self.stop();
    }
}
fn run(source: SourcePtr, state: &Shared, folder: &Path) -> Result<()> {
    ensure!(
        !folder.as_os_str().is_empty(),
        "録音フォルダーを選んでください"
    );
    let mut reader = RecordingStream::open(folder, 4800, &state.cancel, &library()?)?;
    let mut primed: std::collections::VecDeque<f32> = reader.read(5280, &state.cancel)?.into();
    if state.cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    state.duration.store(reader.frames(), Ordering::Relaxed);
    state
        .state
        .store(obs_media_state_OBS_MEDIA_STATE_PLAYING, Ordering::Relaxed);
    unsafe {
        (api().obs_source_media_started)(source.0);
    }
    let mut first = 0u64;
    let mut anchor = clock::now_ns();
    let mut level = Level::default();
    let mut paused = false;
    while !state.cancel.load(Ordering::Relaxed) {
        if state.paused.load(Ordering::Relaxed) {
            paused = true;
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let now = clock::now_ns();
        let offset = (first * 1_000_000_000 / 48000) as i64;
        if paused || now > anchor + offset + 250_000_000 {
            anchor = now - offset;
            paused = false;
        }
        let target = anchor + offset;
        if now < target {
            thread::sleep(Duration::from_nanos((target - now).min(5_000_000) as u64));
            continue;
        }
        let mut pcm = if !primed.is_empty() {
            let n = primed.len().min(960);
            primed.drain(..n).collect()
        } else {
            reader.read(480, &state.cancel)?
        };
        let n = pcm.len() / 2;
        if n == 0 {
            break;
        }
        level.apply(&mut pcm, f64::from_bits(state.gain.load(Ordering::Relaxed)))?;
        source.output(&pcm, target + 100_000_000)?;
        first += n as u64;
        state.cursor.store(
            first.saturating_sub(4800).min(reader.frames()),
            Ordering::Relaxed,
        );
    }
    Ok(())
}
unsafe extern "C" fn name(_: *mut c_void) -> *const std::ffi::c_char {
    c"Virtual Spatial Mic（記録再生）".as_ptr()
}
unsafe extern "C" fn defaults(d: *mut obs_data_t) {
    guard(|| (api().obs_data_set_default_double)(d, c"gain_db".as_ptr(), -18.))
}
unsafe extern "C" fn create(d: *mut obs_data_t, s: *mut obs_source_t) -> *mut c_void {
    guard(|| {
        let p = Box::new(Replay {
            source: SourcePtr(s),
            control: Mutex::new(Control {
                path: PathBuf::new(),
                worker: None,
            }),
            state: Arc::new(Shared {
                cancel: AtomicBool::new(false),
                paused: AtomicBool::new(false),
                cursor: AtomicU64::new(0),
                duration: AtomicU64::new(0),
                gain: AtomicU64::new(1f64.to_bits()),
                state: AtomicI32::new(obs_media_state_OBS_MEDIA_STATE_STOPPED),
                error: Mutex::new(String::new()),
            }),
        });
        p.update(d);
        Box::into_raw(p).cast()
    })
}
unsafe extern "C" fn destroy(p: *mut c_void) {
    guard(|| drop(Box::from_raw(p as *mut Replay)))
}
unsafe extern "C" fn update(p: *mut c_void, d: *mut obs_data_t) {
    guard(|| (*(p as *mut Replay)).update(d))
}
unsafe extern "C" fn play(p: *mut c_void) {
    guard(|| (*(p as *mut Replay)).start())
}
unsafe extern "C" fn pause(p: *mut c_void, pause: bool) {
    guard(|| (*(p as *mut Replay)).pause(pause))
}
unsafe extern "C" fn stop(p: *mut c_void) {
    guard(|| (*(p as *mut Replay)).stop())
}
unsafe extern "C" fn duration(p: *mut c_void) -> i64 {
    guard(|| {
        (&*(p as *mut Replay))
            .state
            .duration
            .load(Ordering::Relaxed) as i64
            / 48
    })
}
unsafe extern "C" fn time(p: *mut c_void) -> i64 {
    guard(|| (&*(p as *mut Replay)).state.cursor.load(Ordering::Relaxed) as i64 / 48)
}
unsafe extern "C" fn state(p: *mut c_void) -> obs_media_state {
    guard(|| (&*(p as *mut Replay)).state.state.load(Ordering::Relaxed))
}
unsafe extern "C" fn play_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if !p.is_null() {
            play(p);
        }
        true
    })
}
unsafe extern "C" fn pause_button(
    _: *mut obs_properties_t,
    _: *mut obs_property_t,
    p: *mut c_void,
) -> bool {
    guard(|| {
        if !p.is_null() {
            let s = &*(p as *mut Replay);
            s.pause(!s.state.paused.load(Ordering::Relaxed));
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
        if !p.is_null() {
            stop(p);
        }
        true
    })
}
unsafe extern "C" fn properties(data: *mut c_void) -> *mut obs_properties_t {
    guard(|| {
        let a = api();
        let p = (a.obs_properties_create)();
        text(
            p,
            c"about",
            "原音と位置データから再処理し、OBSで再生します。\n原音がない場合は保存済みの処理音声を使います。",
        );
        (a.obs_properties_add_path)(
            p,
            c"recording".as_ptr(),
            c"録音フォルダー".as_ptr(),
            obs_path_type_OBS_PATH_DIRECTORY,
            std::ptr::null(),
            std::ptr::null(),
        );
        (a.obs_properties_add_float_slider)(
            p,
            c"gain_db".as_ptr(),
            c"入力レベル (dB)".as_ptr(),
            -60.,
            0.,
            1.,
        );
        button(p, c"play", "最初から再生", Some(play_button), data);
        button(p, c"pause", "一時停止 / 再開", Some(pause_button), data);
        button(p, c"stop", "停止", Some(stop_button), data);
        if !data.is_null() {
            let s = &*(data as *mut Replay);
            let error = s.state.error.lock().unwrap();
            if !error.is_empty() {
                text(p, c"last_error", &error);
            }
        }
        p
    })
}
pub unsafe fn register() {
    let info = obs_source_info {
        id: c"vbs_recording".as_ptr(),
        type_: obs_source_type_OBS_SOURCE_TYPE_INPUT,
        output_flags: OBS_SOURCE_AUDIO | OBS_SOURCE_CONTROLLABLE_MEDIA,
        get_name: Some(name),
        create: Some(create),
        destroy: Some(destroy),
        get_defaults: Some(defaults),
        get_properties: Some(properties),
        update: Some(update),
        media_play_pause: Some(pause),
        media_restart: Some(play),
        media_stop: Some(stop),
        media_get_duration: Some(duration),
        media_get_time: Some(time),
        media_get_state: Some(state),
        ..Default::default()
    };
    (api().obs_register_source_s)(&info, std::mem::size_of_val(&info));
}
