//! Small, owned FFI boundary to the pinned third-party Steam Audio C API.
//! All stream policy and gain processing below are Rust; no project C++ library
//! is loaded. One processor belongs to one audio worker.
use crate::math::{Vec3, norm};
use anyhow::{Context, Result, ensure};
use libloading::Library;
use std::{
    ffi::{c_char, c_void},
    path::{Path, PathBuf},
    ptr,
};

type Handle = *mut c_void;
#[repr(C)]
struct ContextSettings {
    version: u32,
    log: Option<unsafe extern "C" fn(i32, *const c_char)>,
    allocate: Option<unsafe extern "C" fn(usize, usize) -> Handle>,
    free: Option<unsafe extern "C" fn(Handle)>,
    simd: i32,
    flags: i32,
}
#[repr(C)]
struct AudioSettings {
    rate: i32,
    block: i32,
}
#[repr(C)]
struct HrtfSettings {
    kind: i32,
    file: *const c_char,
    data: *const u8,
    size: i32,
    volume: f32,
    norm: i32,
}
#[repr(C)]
struct EffectSettings {
    hrtf: Handle,
}
#[repr(C)]
struct Params {
    direction: [f32; 3],
    interpolation: i32,
    blend: f32,
    hrtf: Handle,
    delays: *mut f32,
}
#[repr(C)]
struct AudioBuffer {
    channels: i32,
    samples: i32,
    data: *mut *mut f32,
}
struct Api {
    context_create: unsafe extern "C" fn(*mut ContextSettings, *mut Handle) -> i32,
    context_release: unsafe extern "C" fn(*mut Handle),
    hrtf_create:
        unsafe extern "C" fn(Handle, *mut AudioSettings, *mut HrtfSettings, *mut Handle) -> i32,
    hrtf_release: unsafe extern "C" fn(*mut Handle),
    effect_create:
        unsafe extern "C" fn(Handle, *mut AudioSettings, *mut EffectSettings, *mut Handle) -> i32,
    effect_release: unsafe extern "C" fn(*mut Handle),
    apply: unsafe extern "C" fn(Handle, *mut Params, *mut AudioBuffer, *mut AudioBuffer) -> i32,
    reset: unsafe extern "C" fn(Handle),
    tail_size: unsafe extern "C" fn(Handle) -> i32,
    tail: unsafe extern "C" fn(Handle, *mut AudioBuffer) -> i32,
    _library: Library,
}
impl Api {
    fn load(path: &Path) -> Result<Self> {
        // Only a local, explicit dependency path is loaded; function pointers
        // never outlive the library retained in this object.
        unsafe {
            let lib = Library::new(path)
                .with_context(|| format!("Steam Audioを読み込めません: {}", path.display()))?;
            Ok(Self {
                context_create: *lib.get(b"iplContextCreate\0")?,
                context_release: *lib.get(b"iplContextRelease\0")?,
                hrtf_create: *lib.get(b"iplHRTFCreate\0")?,
                hrtf_release: *lib.get(b"iplHRTFRelease\0")?,
                effect_create: *lib.get(b"iplBinauralEffectCreate\0")?,
                effect_release: *lib.get(b"iplBinauralEffectRelease\0")?,
                apply: *lib.get(b"iplBinauralEffectApply\0")?,
                reset: *lib.get(b"iplBinauralEffectReset\0")?,
                tail_size: *lib.get(b"iplBinauralEffectGetTailSize\0")?,
                tail: *lib.get(b"iplBinauralEffectGetTail\0")?,
                _library: lib,
            })
        }
    }
}
pub fn packaged_library() -> Result<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "phonon.dll"
    } else if cfg!(target_os = "macos") {
        "libphonon.dylib"
    } else {
        "libphonon.so"
    };
    Ok(std::env::current_exe()?
        .parent()
        .context("Executable has no directory")?
        .join(name))
}
#[derive(Clone, Copy, PartialEq)]
enum State {
    Accepting,
    Draining,
    Complete,
}
pub struct Binaural {
    api: Api,
    context: Handle,
    hrtf: Handle,
    effect: Handle,
    params: Params,
    block: usize,
    bounded_distance: bool,
    mono: Vec<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
    state: State,
    previous_gain: f64,
    has_input: bool,
}
impl Binaural {
    pub fn new(library: &Path, block: usize, bounded_distance: bool) -> Result<Self> {
        ensure!(
            [128, 256, 512, 1024].contains(&block),
            "Supported block sizes: 128,256,512,1024"
        );
        let api = Api::load(library)?;
        let mut result = Self {
            api,
            context: ptr::null_mut(),
            hrtf: ptr::null_mut(),
            effect: ptr::null_mut(),
            params: Params {
                direction: [0.; 3],
                interpolation: 1,
                blend: 1.,
                hrtf: ptr::null_mut(),
                delays: ptr::null_mut(),
            },
            block,
            bounded_distance,
            mono: vec![0.; block],
            left: vec![0.; block],
            right: vec![0.; block],
            state: State::Accepting,
            previous_gain: 1.,
            has_input: false,
        };
        let mut settings = ContextSettings {
            version: 0x040801,
            log: None,
            allocate: None,
            free: None,
            simd: 0,
            flags: 0,
        };
        let mut audio = AudioSettings {
            rate: 48000,
            block: block as i32,
        };
        let mut hrtf = HrtfSettings {
            kind: 0,
            file: ptr::null(),
            data: ptr::null(),
            size: 0,
            volume: 1.,
            norm: 0,
        };
        unsafe {
            ensure!(
                (result.api.context_create)(&mut settings, &mut result.context) == 0,
                "iplContextCreate failed"
            );
            ensure!(
                (result.api.hrtf_create)(result.context, &mut audio, &mut hrtf, &mut result.hrtf)
                    == 0,
                "iplHRTFCreate failed"
            );
            ensure!(
                (result.api.effect_create)(
                    result.context,
                    &mut audio,
                    &mut EffectSettings { hrtf: result.hrtf },
                    &mut result.effect
                ) == 0,
                "iplBinauralEffectCreate failed"
            );
        }
        result.params.hrtf = result.hrtf;
        Ok(result)
    }
    fn apply(&mut self) {
        let mut input = [self.mono.as_mut_ptr()];
        let mut output = [self.left.as_mut_ptr(), self.right.as_mut_ptr()];
        let mut input = AudioBuffer {
            channels: 1,
            samples: self.block as i32,
            data: input.as_mut_ptr(),
        };
        let mut output = AudioBuffer {
            channels: 2,
            samples: self.block as i32,
            data: output.as_mut_ptr(),
        };
        // Channel storage has exactly block samples and remains live throughout
        // the synchronous SDK call. The SDK does not retain these buffers.
        unsafe {
            (self.api.apply)(self.effect, &mut self.params, &mut input, &mut output);
        }
    }
    fn interleave(&self, output: &mut [f32], frames: usize) {
        for i in 0..frames {
            output[i * 2] = self.left[i];
            output[i * 2 + 1] = self.right[i];
        }
    }
    fn finite(&self) -> bool {
        self.left.iter().chain(&self.right).all(|v| v.is_finite())
    }
    pub fn process(&mut self, input: &[f32], output: &mut [f32], position: Vec3) -> Result<()> {
        output.fill(0.);
        ensure!(
            input.len() == self.block && output.len() == self.block * 2,
            "Invalid DSP block size"
        );
        ensure!(self.state == State::Accepting, "Stream finished");
        let distance = norm(position);
        ensure!(
            position.iter().all(|v| v.is_finite()) && distance.is_finite() && distance >= 1e-6,
            "Invalid source position"
        );
        ensure!(input.iter().all(|v| v.is_finite()), "Nonfinite audio");
        let gain = if self.bounded_distance {
            4f64.min(1. / distance.max(0.25))
        } else {
            1.
        };
        let previous = if self.has_input {
            self.previous_gain
        } else {
            gain
        };
        for (i, v) in input.iter().enumerate() {
            self.mono[i] = (*v as f64
                * (previous + (gain - previous) * (i + 1) as f64 / self.block as f64))
                as f32;
            ensure!(self.mono[i].is_finite(), "Gain overflow");
        }
        self.params.direction = position.map(|v| (v / distance) as f32);
        self.apply();
        if !self.finite() {
            self.reset();
            anyhow::bail!("Nonfinite SDK output");
        }
        self.interleave(output, self.block);
        self.previous_gain = gain;
        self.has_input = true;
        Ok(())
    }
    pub fn drain(&mut self, output: &mut [f32]) -> Result<(usize, bool)> {
        output.fill(0.);
        ensure!(output.len() == self.block * 2, "Invalid DSP block size");
        if !self.has_input || self.state == State::Complete {
            self.state = State::Complete;
            return Ok((0, true));
        }
        if self.state == State::Accepting {
            self.mono.fill(0.);
            self.apply();
            if !self.finite() {
                self.reset();
                anyhow::bail!("Nonfinite SDK tail");
            }
            self.interleave(output, self.block);
            let complete = unsafe { (self.api.tail_size)(self.effect) } <= 0;
            self.state = if complete {
                State::Complete
            } else {
                State::Draining
            };
            return Ok((self.block, complete));
        }
        let remaining = unsafe { (self.api.tail_size)(self.effect) };
        if remaining <= 0 {
            self.state = State::Complete;
            return Ok((0, true));
        }
        let mut channels = [self.left.as_mut_ptr(), self.right.as_mut_ptr()];
        let mut buffer = AudioBuffer {
            channels: 2,
            samples: self.block as i32,
            data: channels.as_mut_ptr(),
        };
        let complete = unsafe { (self.api.tail)(self.effect, &mut buffer) } == 1;
        if !self.finite() {
            self.reset();
            anyhow::bail!("Nonfinite SDK tail");
        }
        let frames = self.block.min(remaining as usize);
        self.interleave(output, frames);
        if complete {
            self.state = State::Complete;
        }
        Ok((frames, complete))
    }
    pub fn reset(&mut self) {
        unsafe {
            (self.api.reset)(self.effect);
        }
        self.mono.fill(0.);
        self.left.fill(0.);
        self.right.fill(0.);
        self.state = State::Accepting;
        self.previous_gain = 1.;
        self.has_input = false;
    }
    pub fn latency_frames(&self) -> usize {
        self.block / 4
    }
}
impl Drop for Binaural {
    fn drop(&mut self) {
        unsafe {
            if !self.effect.is_null() {
                (self.api.effect_release)(&mut self.effect);
            }
            if !self.hrtf.is_null() {
                (self.api.hrtf_release)(&mut self.hrtf);
            }
            if !self.context.is_null() {
                (self.api.context_release)(&mut self.context);
            }
        }
    }
}

pub struct Level {
    envelope: f64,
    pub limited_frames: u64,
}
impl Default for Level {
    fn default() -> Self {
        Self {
            envelope: 1.,
            limited_frames: 0,
        }
    }
}
impl Level {
    pub fn apply(&mut self, stereo: &mut [f32], gain: f64) -> Result<()> {
        self.apply_ramp(stereo, gain, gain)
    }
    pub fn apply_ramp(&mut self, stereo: &mut [f32], from: f64, gain: f64) -> Result<()> {
        ensure!(
            stereo.len() % 2 == 0
                && gain.is_finite()
                && (0.0..=1.0).contains(&gain)
                && from.is_finite()
                && (0.0..=1.0).contains(&from),
            "Invalid output level"
        );
        let frames = stereo.len() / 2;
        for (i, pair) in stereo.chunks_exact_mut(2).enumerate() {
            let gain = from + (gain - from) * (i + 1) as f64 / frames as f64;
            let l = pair[0] as f64 * gain;
            let r = pair[1] as f64 * gain;
            ensure!(l.is_finite() && r.is_finite(), "Nonfinite output");
            let peak = l.abs().max(r.abs());
            let target = if peak > 0.98 { 0.98 / peak } else { 1. };
            self.envelope = target.min(self.envelope + (1. - self.envelope) / 2400.);
            if self.envelope < 1. {
                self.limited_frames += 1;
            }
            pair[0] = (l * self.envelope) as f32;
            pair[1] = (r * self.envelope) as f32;
        }
        Ok(())
    }
}
