//! Minimal read-only implementation of the Spout2 shared D3D11 texture protocol.
//! Protocol layout and synchronization follow leadedge/Spout2, commit
//! c2bcc12147711d12ace7d5f08e869d774d840f8a. No Spout SDK binary is required.

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{ffi::c_void, mem::size_of};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_ABANDONED, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        System::{Memory::*, Threading::*},
    },
    core::{Interface, PCWSTR},
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Mapping {
    _handle: OwnedHandle,
    pointer: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}
impl Mapping {
    fn open(name: &str) -> Result<Self> {
        let name = wide(name);
        unsafe {
            let handle = OwnedHandle(OpenFileMappingW(
                FILE_MAP_READ.0,
                false,
                PCWSTR(name.as_ptr()),
            )?);
            let pointer = MapViewOfFile(handle.0, FILE_MAP_READ, 0, 0, 0);
            ensure!(
                !pointer.Value.is_null(),
                "Could not map Spout shared memory"
            );
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = VirtualQuery(
                Some(pointer.Value),
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            if queried == 0 {
                let _ = UnmapViewOfFile(pointer);
                anyhow::bail!("Could not inspect Spout mapping length");
            }
            Ok(Self {
                _handle: handle,
                pointer,
                len: info.RegionSize.min(1024 * 1024),
            })
        }
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.pointer.Value as *const u8, self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.pointer);
        }
    }
}
struct MutexGuard(OwnedHandle);
impl MutexGuard {
    fn open(name: &str) -> Result<Self> {
        let name = wide(name);
        unsafe {
            let handle = OwnedHandle(CreateMutexW(None, false, PCWSTR(name.as_ptr()))?);
            let wait = WaitForSingleObject(handle.0, 67);
            ensure!(
                wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED,
                "Spout texture mutex timed out"
            );
            Ok(Self(handle))
        }
    }
}
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0.0);
        }
    }
}

fn string(slot: &[u8]) -> String {
    String::from_utf8_lossy(&slot[..slot.iter().position(|b| *b == 0).unwrap_or(slot.len())])
        .into_owned()
}
fn sender_names() -> Result<Vec<String>> {
    let mapping = match Mapping::open("SpoutSenderNames") {
        Ok(map) => map,
        Err(_) => return Ok(Vec::new()),
    };
    let _lock = MutexGuard::open("SpoutSenderNames_mutex")?;
    Ok(mapping
        .bytes()
        .chunks_exact(256)
        .map(string)
        .take_while(|n| !n.is_empty())
        .collect())
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct TextureInfo {
    share_handle: u32,
    width: u32,
    height: u32,
    format: u32,
}
fn info(name: &str) -> Result<TextureInfo> {
    let mapping = Mapping::open(name).context("Spout sender information is unavailable")?;
    let _lock = MutexGuard::open(&format!("{name}_mutex"))?;
    ensure!(mapping.len >= 280, "Spout sender information is truncated");
    let bytes = mapping.bytes();
    let word = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    Ok(TextureInfo {
        share_handle: word(0),
        width: word(4),
        height: word(8),
        format: word(12),
    })
}
#[derive(Clone, Debug, Serialize)]
pub struct SenderInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub active: bool,
}
pub fn list_senders() -> Result<Vec<SenderInfo>> {
    let active = Mapping::open("ActiveSenderName")
        .ok()
        .and_then(|map| {
            let _lock = MutexGuard::open("ActiveSenderName_mutex").ok()?;
            Some(string(&map.bytes()[..map.len.min(256)]))
        })
        .unwrap_or_default();
    Ok(sender_names()?
        .into_iter()
        .filter_map(|name| {
            info(&name).ok().map(|i| SenderInfo {
                active: name == active,
                name,
                width: i.width,
                height: i.height,
                format: i.format,
            })
        })
        .collect())
}

struct KeyGuard(IDXGIKeyedMutex);
impl Drop for KeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseSync(0);
        }
    }
}
enum Access {
    Named(MutexGuard),
    Keyed(KeyGuard),
}
impl Drop for Access {
    fn drop(&mut self) {
        // Touch the guards so ownership is explicit; their own Drop releases the lock.
        match self {
            Self::Named(guard) => {
                let _ = &guard.0;
            }
            Self::Keyed(guard) => {
                let _ = &guard.0;
            }
        }
    }
}

pub(crate) struct Receiver {
    name: String,
    original: TextureInfo,
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    shared: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    keyed: Option<IDXGIKeyedMutex>,
    semaphore: Option<OwnedHandle>,
    frame_count: Option<i32>,
    width: u32,
    height: u32,
    rgba: bool,
}
impl Receiver {
    pub fn open(name: &str) -> Result<Self> {
        ensure!(
            !name.is_empty() && name.len() < 256 && !name.contains('\0'),
            "Invalid Spout sender name"
        );
        let original = info(name)?;
        ensure!(
            original.share_handle != 0,
            "Spout sender has no shared texture"
        );
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut selected = None;
        let mut failures = Vec::new();
        for index in 0..16 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
                break;
            };
            let mut device = None;
            let mut context = None;
            let result = unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            };
            if let Err(error) = result {
                failures.push(error.to_string());
                continue;
            }
            let device = device.unwrap();
            let mut shared: Option<ID3D11Texture2D> = None;
            // The SDK uses LongToHandle: preserve signed LONG extension on x64.
            let handle = HANDLE(original.share_handle as i32 as isize as *mut c_void);
            match unsafe { device.OpenSharedResource(handle, &mut shared) } {
                Ok(()) => {
                    selected = Some((device, context.unwrap(), shared.unwrap()));
                    break;
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        let (device, context, shared) = selected.with_context(|| {
            format!(
                "Spout texture could not be opened on any GPU: {}",
                failures.join("; ")
            )
        })?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            shared.GetDesc(&mut desc);
        }
        let width = desc.Width & !1;
        let height = desc.Height & !1;
        ensure!(
            (16..=4096).contains(&width) && (16..=4096).contains(&height),
            "Spout resolution must be 16–4096 pixels"
        );
        let rgba = desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM
            || desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
        ensure!(
            rgba || desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM
                || desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            "Unsupported Spout texture format: {}",
            desc.Format.0
        );
        ensure!(
            desc.SampleDesc.Count == 1 && desc.ArraySize == 1,
            "Multisampled or array Spout textures are unsupported"
        );
        let keyed = if desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0 {
            Some(shared.cast()?)
        } else {
            None
        };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        let semaphore_name = wide(&format!("{name}_Count_Semaphore"));
        let semaphore =
            unsafe { OpenSemaphoreW(SEMAPHORE_ALL_ACCESS, false, PCWSTR(semaphore_name.as_ptr())) }
                .ok()
                .map(OwnedHandle);
        Ok(Self {
            name: name.into(),
            original,
            _device: device,
            context,
            shared,
            staging: staging.unwrap(),
            keyed,
            semaphore,
            frame_count: None,
            width,
            height,
            rgba,
        })
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn read(&mut self) -> Result<Option<Vec<u8>>> {
        ensure!(
            sender_names()?.iter().any(|n| n == &self.name),
            "Spout映像送信元が終了しました"
        );
        ensure!(
            info(&self.name)? == self.original,
            "Spout映像の解像度または共有テクスチャが変更されました。録画を開始し直してください"
        );
        let _access = if let Some(keyed) = &self.keyed {
            // WAIT_TIMEOUT / WAIT_ABANDONED are positive HRESULT values, so a
            // generic Result<()> wrapper would incorrectly count them as success.
            let acquired =
                unsafe { (Interface::vtable(keyed).AcquireSync)(Interface::as_raw(keyed), 0, 67) };
            ensure!(
                acquired.0 == 0,
                "Spout keyed texture mutex timed out or was abandoned"
            );
            Access::Keyed(KeyGuard(keyed.clone()))
        } else {
            Access::Named(MutexGuard::open(&format!(
                "{}_SpoutAccessMutex",
                self.name
            ))?)
        };
        if let Some(semaphore) = &self.semaphore {
            if unsafe { WaitForSingleObject(semaphore.0, 0) } == WAIT_OBJECT_0 {
                let mut count = 0;
                unsafe {
                    ReleaseSemaphore(semaphore.0, 1, Some(&mut count))?;
                }
                if self.frame_count == Some(count) {
                    return Ok(None);
                }
                self.frame_count = Some(count);
            }
        }
        unsafe {
            self.context.CopyResource(&self.staging, &self.shared);
            self.context.Flush();
        }
        // Hold the sender lock only through the GPU copy submission, as the SDK does.
        drop(_access);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let read = (|| -> Result<Vec<u8>> {
            ensure!(
                !mapped.pData.is_null() && mapped.RowPitch >= self.width * 4,
                "Invalid Spout readback layout"
            );
            let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
            for y in 0..self.height as usize {
                let source = unsafe {
                    std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                        self.width as usize * 4,
                    )
                };
                let dest =
                    &mut pixels[y * self.width as usize * 4..(y + 1) * self.width as usize * 4];
                dest.copy_from_slice(source);
                for p in dest.chunks_exact_mut(4) {
                    if self.rgba {
                        p.swap(0, 2);
                    }
                    p[3] = 255;
                }
            }
            Ok(pixels)
        })();
        unsafe {
            self.context.Unmap(&self.staging, 0);
        }
        read.map(Some)
    }
}
