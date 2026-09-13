//! C ABI boundary. OBS owns the opaque pointers; each source joins all workers
//! before its destroy callback returns. Resolve only already-loaded host DLLs.
#![allow(unsafe_op_in_unsafe_fn, non_snake_case)]
use crate::bindings::*;
use anyhow::{Context, Result};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicPtr, Ordering},
    },
};
pub type Button =
    Option<unsafe extern "C" fn(*mut obs_properties_t, *mut obs_property_t, *mut c_void) -> bool>;
pub type Event = unsafe extern "C" fn(obs_frontend_event, *mut c_void);
pub static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
pub static API: OnceLock<Api> = OnceLock::new();
macro_rules! api {
    ($(fn $name:ident($($arg:ty),*) $(->$ret:ty)?;)+)=>{
        pub struct Api { _library:Library, $(pub $name:unsafe extern "C" fn($($arg),*) $(->$ret)?,)+ pub frontend:Option<Frontend> }
        impl Api { pub fn load()->Result<Self>{let library:Library=libloading::os::windows::Library::open_already_loaded("obs.dll")?.into();unsafe{Ok(Self{$($name:*library.get(concat!(stringify!($name),"\0").as_bytes())?,)+frontend:Frontend::load().ok(),_library:library})}} }
    }
}
api! {
 fn obs_register_source_s(*const obs_source_info,usize);
 fn obs_get_version()->u32;
 fn obs_source_output_audio(*mut obs_source_t,*const obs_source_audio);
 fn obs_source_active(*const obs_source_t)->bool;
 fn obs_source_update_properties(*mut obs_source_t);
 fn obs_source_media_started(*mut obs_source_t);
 fn obs_source_media_ended(*mut obs_source_t);
 fn obs_source_get_name(*const obs_source_t)->*const c_char;
 fn obs_get_module_binary_path(*mut c_void)->*const c_char;
 fn obs_data_get_string(*mut obs_data_t,*const c_char)->*const c_char;
 fn obs_data_get_int(*mut obs_data_t,*const c_char)->i64;
 fn obs_data_get_double(*mut obs_data_t,*const c_char)->f64;
 fn obs_data_get_bool(*mut obs_data_t,*const c_char)->bool;
 fn obs_data_set_default_double(*mut obs_data_t,*const c_char,f64);
 fn obs_data_set_default_int(*mut obs_data_t,*const c_char,i64);
 fn obs_data_set_default_bool(*mut obs_data_t,*const c_char,bool);
 fn obs_properties_create()->*mut obs_properties_t;

 fn obs_properties_add_text(*mut obs_properties_t,*const c_char,*const c_char,obs_text_type)->*mut obs_property_t;
 fn obs_properties_add_list(*mut obs_properties_t,*const c_char,*const c_char,obs_combo_type,obs_combo_format)->*mut obs_property_t;
 fn obs_property_list_add_string(*mut obs_property_t,*const c_char,*const c_char)->usize;
 fn obs_property_list_add_int(*mut obs_property_t,*const c_char,i64)->usize;
 fn obs_properties_add_path(*mut obs_properties_t,*const c_char,*const c_char,obs_path_type,*const c_char,*const c_char)->*mut obs_property_t;
 fn obs_properties_add_float_slider(*mut obs_properties_t,*const c_char,*const c_char,f64,f64,f64)->*mut obs_property_t;
 fn obs_properties_add_bool(*mut obs_properties_t,*const c_char,*const c_char)->*mut obs_property_t;
 fn obs_properties_add_button2(*mut obs_properties_t,*const c_char,*const c_char,Button,*mut c_void)->*mut obs_property_t;
 fn obs_properties_get(*mut obs_properties_t,*const c_char)->*mut obs_property_t;
 fn obs_property_set_enabled(*mut obs_property_t,bool);
 fn bfree(*mut c_void);
}
pub struct Frontend {
    _library: Library,
    pub add: unsafe extern "C" fn(Event, *mut c_void),
    pub remove: unsafe extern "C" fn(Event, *mut c_void),
    pub recording: unsafe extern "C" fn() -> bool,
    pub last: unsafe extern "C" fn() -> *mut c_char,
}
impl Frontend {
    fn load() -> Result<Self> {
        let library: Library =
            libloading::os::windows::Library::open_already_loaded("obs-frontend-api.dll")?.into();
        unsafe {
            Ok(Self {
                add: *library.get(b"obs_frontend_add_event_callback\0")?,
                remove: *library.get(b"obs_frontend_remove_event_callback\0")?,
                recording: *library.get(b"obs_frontend_recording_active\0")?,
                last: *library.get(b"obs_frontend_get_last_recording\0")?,
                _library: library,
            })
        }
    }
}
pub fn api() -> &'static Api {
    API.get().expect("OBS module is initialized")
}
pub fn c(value: &str) -> CString {
    CString::new(value.replace('\0', "�")).unwrap()
}
pub unsafe fn string(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        CStr::from_ptr(value).to_string_lossy().into_owned()
    }
}
pub unsafe fn setting(data: *mut obs_data_t, key: &CStr) -> String {
    string((api().obs_data_get_string)(data, key.as_ptr()))
}
pub fn library() -> Result<PathBuf> {
    unsafe {
        let path = string((api().obs_get_module_binary_path)(
            MODULE.load(Ordering::Relaxed),
        ));
        Ok(Path::new(&path)
            .parent()
            .context("OBS plugin folder missing")?
            .join("phonon.dll"))
    }
}
pub fn log(message: &str) {
    if let Some(api) = API.get() {
        unsafe {
            if let Ok(blog) = api
                ._library
                .get::<unsafe extern "C" fn(i32, *const c_char, ...)>(b"blog\0")
            {
                blog(300, c"[VSM Rust] %s".as_ptr(), c(message).as_ptr());
            }
        }
    }
}
pub fn guard<T: Default>(f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            log("Callback panicked; exception contained at OBS boundary");
            T::default()
        }
    }
}
/// Pointer is usable from audio workers by the OBS source API contract. The
/// owning source must stop/join the workers before OBS releases this pointer.
#[derive(Clone, Copy)]
pub struct SourcePtr(pub *mut obs_source_t);
unsafe impl Send for SourcePtr {}
unsafe impl Sync for SourcePtr {}
impl SourcePtr {
    pub fn output(self, samples: &[f32], timestamp: i64) -> Result<()> {
        anyhow::ensure!(
            samples.len() % 2 == 0 && timestamp >= 0,
            "Invalid OBS audio packet"
        );
        let mut p = obs_source_audio {
            frames: (samples.len() / 2) as u32,
            speakers: speaker_layout_SPEAKERS_STEREO,
            format: audio_format_AUDIO_FORMAT_FLOAT,
            samples_per_sec: 48000,
            timestamp: timestamp as u64,
            ..Default::default()
        };
        p.data[0] = samples.as_ptr().cast();
        unsafe {
            (api().obs_source_output_audio)(self.0, &p);
        }
        Ok(())
    }
    pub fn changed(self) {
        unsafe {
            (api().obs_source_update_properties)(self.0);
        }
    }
}
pub unsafe fn text(p: *mut obs_properties_t, key: &CStr, value: &str) {
    (api().obs_properties_add_text)(
        p,
        key.as_ptr(),
        c(value).as_ptr(),
        obs_text_type_OBS_TEXT_INFO,
    );
}
pub unsafe fn button(
    p: *mut obs_properties_t,
    key: &CStr,
    label: &str,
    f: Button,
    data: *mut c_void,
) {
    (api().obs_properties_add_button2)(p, key.as_ptr(), c(label).as_ptr(), f, data);
}
pub unsafe fn enabled(p: *mut obs_properties_t, key: &CStr, value: bool) {
    (api().obs_property_set_enabled)((api().obs_properties_get)(p, key.as_ptr()), value);
}
