use crate::{capture::CapturedPacket, clock};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use vsm_core::processor::AudioPacket;
use windows::{
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::*,
        Media::Audio::*,
        System::{
            Com::{StructuredStorage::*, *},
            Threading::*,
        },
    },
    core::{GUID, HSTRING, Interface, PWSTR},
};

struct Com {
    initialized: bool,
}
impl Com {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr != RPC_E_CHANGED_MODE {
            hr.ok()?;
        }
        Ok(Self {
            initialized: hr.is_ok(),
        })
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
struct Event(HANDLE);
impl Event {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Mix(*mut WAVEFORMATEX);
impl Drop for Mix {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}
fn allocated_string(p: PWSTR) -> Result<String> {
    let result = unsafe { p.to_string() };
    unsafe {
        CoTaskMemFree(Some(p.0.cast()));
    }
    Ok(result?)
}
fn id(device: &IMMDevice) -> Result<String> {
    allocated_string(unsafe { device.GetId()? })
}
fn enum_devices() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}
fn format(mix: &Mix) -> Result<Value> {
    ensure!(!mix.0.is_null(), "Missing device format");
    let w = unsafe { ptr::read_unaligned(mix.0) };
    let (mut valid, mut mask, mut floating) = (w.wBitsPerSample, 0, w.wFormatTag == 3);
    if w.wFormatTag == 0xfffe && w.cbSize >= 22 {
        let ext = unsafe { ptr::read_unaligned(mix.0.cast::<WAVEFORMATEXTENSIBLE>()) };
        valid = unsafe { ext.Samples.wValidBitsPerSample };
        mask = ext.dwChannelMask;
        let subformat = ext.SubFormat;
        floating = subformat == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    }
    let rate = w.nSamplesPerSec;
    let channels = w.nChannels;
    let bits = w.wBitsPerSample;
    let align = w.nBlockAlign;
    Ok(
        json!({"sample_rate_hz":rate,"channels":channels,"container_bits":bits,"valid_bits":valid,"channel_mask":mask,"file_sample_format":if floating{"float32"}else{"unsupported"},"hardware_path_format":"unknown","block_align":align}),
    )
}
fn supported(fmt: &Value, output: bool) -> bool {
    fmt["sample_rate_hz"] == 48000
        && fmt["container_bits"] == 32
        && fmt["valid_bits"] == 32
        && fmt["file_sample_format"] == "float32"
        && fmt["block_align"].as_u64() == fmt["channels"].as_u64().map(|c| c * 4)
        && if output {
            fmt["channels"] == 2 && (fmt["channel_mask"] == 0 || fmt["channel_mask"] == 3)
        } else {
            fmt["channels"] == 1 || fmt["channels"] == 2
        }
}
pub fn devices() -> Result<Value> {
    let _com = Com::new()?;
    let enumerator = enum_devices()?;
    let mut result = json!({"inputs":[],"outputs":[]});
    for (flow, key) in [(eCapture, "inputs"), (eRender, "outputs")] {
        let default = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
            .ok()
            .and_then(|d| id(&d).ok());
        let list = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { list.GetCount()? } {
            let device = unsafe { list.Item(i)? };
            let endpoint = id(&device)?;
            let mut row = json!({"endpoint_id":endpoint,"name":endpoint,"default":default.as_deref()==Some(&endpoint),"supported":false});
            unsafe {
                if let Ok(store) = device.OpenPropertyStore(STGM_READ) {
                    if let Ok(mut v) = store.GetValue(&PKEY_Device_FriendlyName) {
                        if let Ok(p) = PropVariantToStringAlloc(&v) {
                            if let Ok(name) = allocated_string(p) {
                                row["name"] = json!(name);
                            }
                        }
                        let _ = PropVariantClear(&mut v);
                    }
                }
                if let Ok(client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) {
                    if let Ok(p) = client.GetMixFormat() {
                        let fmt = format(&Mix(p))?;
                        row["supported"] = json!(supported(&fmt, flow == eRender));
                        row["shared_mix_format"] = fmt;
                    }
                }
            }
            result[key].as_array_mut().unwrap().push(row);
        }
    }
    Ok(result)
}
struct CaptureDevice {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    fmt: Value,
    started: bool,
    _com: Com,
}
impl CaptureDevice {
    fn open(endpoint: &str) -> Result<Self> {
        ensure!(!endpoint.is_empty(), "マイクを選んでください");
        let com = Com::new()?;
        let enumerator = enum_devices()?;
        let device = unsafe { enumerator.GetDevice(&HSTRING::from(endpoint))? };
        let direction: IMMEndpoint = device.cast()?;
        ensure!(
            unsafe { direction.GetDataFlow()? } == eCapture,
            "Endpoint is not a microphone"
        );
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let mix = Mix(unsafe { client.GetMixFormat()? });
        let fmt = format(&mix)?;
        ensure!(
            supported(&fmt, false),
            "マイクを共有48 kHz・float32のモノラルまたはステレオに設定してください"
        );
        let event = Event::new()?;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                1_000_000,
                0,
                mix.0,
                None,
            )?;
            client.SetEventHandle(event.0)?;
        }
        let capture = unsafe { client.GetService()? };
        Ok(Self {
            client,
            capture,
            event,
            fmt,
            started: false,
            _com: com,
        })
    }
    fn drain(
        &mut self,
        origin: i64,
        first: &mut u64,
        accept: &mut impl FnMut(CapturedPacket) -> Result<()>,
    ) -> Result<usize> {
        let mut packets = 0;
        let channels = self.fmt["channels"].as_u64().unwrap() as u32;
        while unsafe { self.capture.GetNextPacketSize()? } > 0 {
            let (mut bytes, mut frames, mut flags, mut device, mut qpc) =
                (ptr::null_mut(), 0, 0, 0, 0);
            unsafe {
                self.capture.GetBuffer(
                    &mut bytes,
                    &mut frames,
                    &mut flags,
                    Some(&mut device),
                    Some(&mut qpc),
                )?;
            }
            if frames == 0 {
                break;
            }
            let received = clock::ticks();
            let samples = (|| -> Result<Vec<f32>> {
                ensure!(frames <= 480000, "Capture packet exceeds bounds");
                if flags & 2 != 0 {
                    return Ok(vec![0.; frames as usize * channels as usize]);
                }
                ensure!(!bytes.is_null(), "Missing audio buffer");
                // WASAPI owns the memory until ReleaseBuffer; copy before the
                // queue callback, which must never retain a device pointer.
                Ok(unsafe {
                    std::slice::from_raw_parts(bytes, frames as usize * channels as usize * 4)
                }
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect())
            })();
            unsafe {
                self.capture.ReleaseBuffer(frames)?;
            }
            let samples = samples?;
            ensure!(qpc <= i64::MAX as u64 / 100, "Capture QPC overflow");
            let packet = CapturedPacket {
                audio: AudioPacket {
                    samples,
                    frames,
                    channels,
                    flags,
                    device,
                    file: *first,
                    time: qpc as i64 * 100 - origin,
                },
                qpc_100ns: qpc,
                receive_ticks: received,
            };
            *first += frames as u64;
            accept(packet)?;
            packets += 1;
        }
        Ok(packets)
    }
}
impl Drop for CaptureDevice {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}
pub fn capture(
    endpoint: &str,
    origin: i64,
    seconds: f64,
    cancel: &AtomicBool,
    mut on_format: impl FnMut(Value) -> Result<()>,
    mut accept: impl FnMut(CapturedPacket) -> Result<()>,
) -> Result<Value> {
    let mut device = CaptureDevice::open(endpoint)?;
    on_format(device.fmt.clone())?;
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Capture cancelled before Start"
    );
    unsafe {
        device.client.Start()?;
    }
    device.started = true;
    let start = Instant::now();
    let mut last = Instant::now();
    let mut first = 0;
    while !cancel.load(Ordering::Relaxed)
        && (seconds == 0. || start.elapsed().as_secs_f64() < seconds)
    {
        let wait = unsafe { WaitForSingleObject(device.event.0, 100) };
        ensure!(
            wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT,
            "Capture wait failed"
        );
        if device.drain(origin, &mut first, &mut accept)? > 0 {
            last = Instant::now();
        }
        ensure!(
            last.elapsed() < Duration::from_secs(3),
            "マイクからの入力が3秒停止しました"
        );
    }
    device.drain(origin, &mut first, &mut accept)?;
    unsafe {
        device.client.Stop()?;
    }
    device.started = false;
    Ok(
        json!({"status":"stopped","backend":"wasapi","format":device.fmt,"acquired_frames":first,"qpc_frequency_hz":clock::frequency(),"stop_reason":if cancel.load(Ordering::Relaxed){"cancelled"}else{"duration_limit"}}),
    )
}

pub struct Output {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: Event,
    capacity: u32,
    started: bool,
    _com: Com,
}
impl Output {
    pub fn open(endpoint: &str) -> Result<Self> {
        let com = Com::new()?;
        ensure!(!endpoint.is_empty(), "出力先を選んでください");
        let enumerator = enum_devices()?;
        let device = unsafe { enumerator.GetDevice(&HSTRING::from(endpoint))? };
        let direction: IMMEndpoint = device.cast()?;
        ensure!(
            unsafe { direction.GetDataFlow()? } == eRender,
            "Endpoint is not an output"
        );
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let mix = Mix(unsafe { client.GetMixFormat()? });
        ensure!(
            supported(&format(&mix)?, true),
            "出力先を共有48 kHz・float32ステレオに設定してください"
        );
        let event = Event::new()?;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                400000,
                0,
                mix.0,
                None,
            )?;
            client.SetEventHandle(event.0)?;
        }
        let capacity = unsafe { client.GetBufferSize()? };
        ensure!(
            (1..=48000).contains(&capacity),
            "Unexpected output buffer size"
        );
        let render: IAudioRenderClient = unsafe { client.GetService()? };
        let mut out = Self {
            client,
            render,
            event,
            capacity,
            started: false,
            _com: com,
        };
        unsafe {
            out.render.GetBuffer(capacity)?;
            out.render
                .ReleaseBuffer(capacity, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            out.client.Start()?;
        }
        out.started = true;
        Ok(out)
    }
    pub fn wait(&self) -> Result<Option<(u32, i64)>> {
        let wait = unsafe { WaitForSingleObject(self.event.0, 100) };
        ensure!(
            wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT,
            "Output wait failed"
        );
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        let padding = unsafe { self.client.GetCurrentPadding()? };
        ensure!(padding <= self.capacity, "Invalid output padding");
        Ok(Some((
            self.capacity - padding,
            clock::now_ns() + padding as i64 * 1_000_000_000 / 48000,
        )))
    }
    pub fn write(&self, samples: &[f32]) -> Result<()> {
        ensure!(
            samples.len() % 2 == 0 && samples.len() / 2 <= self.capacity as usize,
            "Invalid output block"
        );
        let count = (samples.len() / 2) as u32;
        if count == 0 {
            return Ok(());
        }
        let p = unsafe { self.render.GetBuffer(count)? };
        ensure!(!p.is_null(), "Missing output buffer");
        unsafe {
            ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), p, samples.len() * 4);
            self.render.ReleaseBuffer(count, 0)?;
        }
        Ok(())
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}
