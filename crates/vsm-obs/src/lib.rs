//! Virtual Spatial Mic OBS adapter. Own processing lives in Rust shared crates.
#[cfg(windows)]
mod api;
#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod bindings;
#[cfg(windows)]
mod live;
#[cfg(windows)]
mod replay;
#[cfg(windows)]
use api::*;
#[cfg(windows)]
use std::{
    ffi::{c_char, c_void},
    sync::atomic::Ordering,
};
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut c_void) {
    MODULE.store(module, Ordering::Relaxed);
}
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    (32 << 24) | (2 << 16) | 2
}
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_name() -> *const c_char {
    c"Virtual Spatial Mic".as_ptr()
}
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_description() -> *const c_char {
    c"Rust binaural recording, memory ring, and OBS recording integration".as_ptr()
}
#[cfg(windows)]
#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    guard(|| match Api::load() {
        Ok(a) => {
            if unsafe { (a.obs_get_version)() } >> 24 != 32 {
                return false;
            }
            let _ = API.set(a);
            unsafe {
                live::register();
                replay::register();
            }
            log("Rust recording and replay sources loaded; no project C++ / Python runtime");
            true
        }
        Err(e) => {
            eprintln!("VSM OBS: {e:#}");
            false
        }
    })
}
