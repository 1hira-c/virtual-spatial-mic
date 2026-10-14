//! Developer-only Spout2 sender for exercising the actual shared-texture receiver.
use anyhow::{Result, ensure};
use std::{
    mem::size_of,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, HANDLE, HMODULE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
        },
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{Common::*, IDXGIResource},
        },
        System::{Memory::*, Threading::*},
    },
    core::{Interface, PCWSTR},
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Mapping {
    _handle: Handle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}
impl Mapping {
    fn create(name: &str, size: u32) -> Result<Self> {
        let name = wide(name);
        unsafe {
            let handle = Handle(CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size,
                PCWSTR(name.as_ptr()),
            )?);
            let view = MapViewOfFile(handle.0, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
            ensure!(
                !view.Value.is_null(),
                "Could not map synthetic sender memory"
            );
            let mut info = MEMORY_BASIC_INFORMATION::default();
            ensure!(
                VirtualQuery(
                    Some(view.Value),
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>()
                ) != 0,
                "Mapping query failed"
            );
            Ok(Self {
                _handle: handle,
                view,
                len: info.RegionSize,
            })
        }
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.view.Value as *mut u8, self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}
struct Guard(Handle);
impl Guard {
    fn open(name: &str) -> Result<Self> {
        let name = wide(name);
        unsafe {
            let handle = Handle(CreateMutexW(None, false, PCWSTR(name.as_ptr()))?);
            let waited = WaitForSingleObject(handle.0, 67);
            ensure!(
                waited == WAIT_OBJECT_0 || waited == WAIT_ABANDONED,
                "Synthetic sender mutex timeout"
            );
            Ok(Self(handle))
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0.0);
        }
    }
}
fn names(mapping: &Mapping) -> Vec<String> {
    mapping
        .bytes()
        .chunks_exact(256)
        .map(|slot| {
            String::from_utf8_lossy(&slot[..slot.iter().position(|v| *v == 0).unwrap_or(256)])
                .into_owned()
        })
        .take_while(|s| !s.is_empty())
        .collect()
}
fn write_names(mapping: &mut Mapping, names: &[String]) -> Result<()> {
    ensure!(
        names.len() < mapping.len / 256,
        "Spout sender registry is full"
    );
    let bytes = mapping.bytes_mut();
    for slot in bytes.chunks_exact_mut(256) {
        slot.fill(0);
    }
    for (index, name) in names.iter().enumerate() {
        ensure!(name.len() < 256, "Spout name too long");
        bytes[index * 256..index * 256 + name.len()].copy_from_slice(name.as_bytes());
    }
    Ok(())
}
struct Sender {
    name: String,
    registry: Mapping,
    _info: Mapping,
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    counter: Handle,
    width: u32,
    height: u32,
}
impl Sender {
    fn open(name: String) -> Result<Self> {
        let (width, height) = (640, 360);
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.unwrap();
        let context = context.unwrap();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        let texture = texture.unwrap();
        let resource: IDXGIResource = texture.cast()?;
        let handle = unsafe { resource.GetSharedHandle()? };
        let mut info = Mapping::create(&name, 280)?;
        {
            let _lock = Guard::open(&format!("{name}_mutex"))?;
            let data = info.bytes_mut();
            data[..280].fill(0);
            data[0..4].copy_from_slice(&(handle.0 as usize as u32).to_le_bytes());
            data[4..8].copy_from_slice(&width.to_le_bytes());
            data[8..12].copy_from_slice(&height.to_le_bytes());
            data[12..16].copy_from_slice(&(DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32).to_le_bytes());
        }
        let mut registry = Mapping::create("SpoutSenderNames", 64 * 256)?;
        {
            let _lock = Guard::open("SpoutSenderNames_mutex")?;
            let mut existing = names(&registry);
            existing.push(name.clone());
            write_names(&mut registry, &existing)?;
        }
        let sem_name = wide(&format!("{name}_Count_Semaphore"));
        let counter =
            Handle(unsafe { CreateSemaphoreW(None, 1, i32::MAX, PCWSTR(sem_name.as_ptr()))? });
        Ok(Self {
            name,
            registry,
            _info: info,
            _device: device,
            context,
            texture,
            counter,
            width,
            height,
        })
    }
    fn frame(&self, index: u64) -> Result<()> {
        let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
        let stripe = (index * 8 % self.width as u64) as u32;
        for y in 0..self.height {
            for x in 0..self.width {
                let at = ((y * self.width + x) * 4) as usize;
                pixels[at] = (x * 255 / self.width) as u8;
                pixels[at + 1] = (y * 255 / self.height) as u8;
                pixels[at + 2] = if x.abs_diff(stripe) < 5 { 255 } else { 0 };
                pixels[at + 3] = 255;
            }
        }
        let _lock = Guard::open(&format!("{}_SpoutAccessMutex", self.name))?;
        unsafe {
            self.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                pixels.as_ptr() as *const _,
                self.width * 4,
                0,
            );
            self.context.Flush();
            if WaitForSingleObject(self.counter.0, 0) == WAIT_OBJECT_0 {
                ReleaseSemaphore(self.counter.0, 2, None)?;
            }
        }
        Ok(())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        if let Ok(_lock) = Guard::open("SpoutSenderNames_mutex") {
            let existing = names(&self.registry)
                .into_iter()
                .filter(|n| n != &self.name)
                .collect::<Vec<_>>();
            let _ = write_names(&mut self.registry, &existing);
        }
    }
}
pub struct RunningSender {
    pub name: String,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
}
impl RunningSender {
    pub fn start() -> Result<Self> {
        let name = format!("VSM Synthetic {}", std::process::id());
        let cancel = Arc::new(AtomicBool::new(false));
        let (worker_name, worker_cancel) = (name.clone(), cancel.clone());
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let sender = Sender::open(worker_name)?;
                sender.frame(0)?;
                let _ = tx.send(Ok(()));
                let mut frame = 1;
                while !worker_cancel.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(33));
                    sender.frame(frame)?;
                    frame += 1;
                }
                Ok(())
            })();
            if let Err(error) = &result {
                let _ = tx.send(Err(error.to_string()));
            }
            result
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                name,
                cancel,
                worker: Some(worker),
            }),
            other => {
                cancel.store(true, Ordering::Relaxed);
                let _ = worker.join();
                anyhow::bail!("Synthetic Spout sender failed: {other:?}")
            }
        }
    }
}
impl Drop for RunningSender {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
