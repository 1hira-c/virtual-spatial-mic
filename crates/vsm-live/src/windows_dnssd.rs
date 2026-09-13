//! Windows owns mDNS discovery reception. VSM advertises separately from a
//! send-only multicast socket because DNSAPI omits the loopback A record.
//! DNS-SD advertisements may still use the LAN. No firewall rules are changed.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    cell::UnsafeCell,
    collections::BTreeMap,
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows::{
    Win32::{Foundation::ERROR_CANCELLED, NetworkManagement::Dns::*},
    core::PCWSTR,
};

const PENDING: u32 = 9506; // DNS_REQUEST_PENDING
type Candidates = Arc<Mutex<BTreeMap<u16, Instant>>>;
type Info = Arc<Mutex<Value>>;

fn retain_callback_module() -> Result<()> {
    // The OBS host may unload a plugin after its sources stop. DNSAPI completes
    // cancellation asynchronously, so its callbacks must remain executable even
    // during that short interval. Pin the owning module until process exit.
    use windows::Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
            GetModuleHandleExW,
        },
    };
    static PIN: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let result = PIN.get_or_init(|| {
        let mut module = HMODULE::default();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                PCWSTR(browsed as *const () as *const u16),
                &mut module,
            )
        }
        .map_err(|e| e.to_string())
    });
    ensure!(
        result.is_ok(),
        "DNS-SD callback module could not be retained: {result:?}"
    );
    Ok(())
}

fn error(info: &Info, operation: &str, status: u32) {
    if let Ok(mut value) = info.lock() {
        value["error"] = json!(format!(
            "WindowsのOSCQuery自動検出に失敗しました（{operation}: {status}）"
        ));
    }
}

struct Browse {
    request: DNS_SERVICE_BROWSE_REQUEST,
    cancel: UnsafeCell<DNS_SERVICE_CANCEL>,
    _query_name: Vec<u16>,
    candidates: Candidates,
    info: Info,
}
// DNSAPI owns asynchronous writes to its cancel handle. Rust never reads it;
// cancellation is invoked once, after DnsServiceBrowse has returned.
unsafe impl Send for Browse {}
unsafe impl Sync for Browse {}

unsafe extern "system" fn browsed(
    status: u32,
    context: *const c_void,
    records: *const DNS_RECORDW,
) {
    let pointer = context.cast::<Browse>();
    let cancelled = status == ERROR_CANCELLED.0;
    if !cancelled {
        // Keep the subscription's raw reference until the terminal cancellation
        // callback; each ordinary callback takes a temporary reference.
        unsafe { Arc::increment_strong_count(pointer) };
    }
    let browse = unsafe { Arc::from_raw(pointer) };
    if status == 0 {
        if let Ok(mut ports) = browse.candidates.lock() {
            let now = Instant::now();
            ports.retain(|_, expiry| *expiry > now);
            let mut row = records;
            for _ in 0..512 {
                if row.is_null() {
                    break;
                }
                let record = unsafe { &*row };
                if record.wType == 33 && !record.pName.is_null() {
                    let name = unsafe { record.pName.to_string() }.unwrap_or_default();
                    if name.to_ascii_lowercase().starts_with("vrchat-client-")
                        && name
                            .to_ascii_lowercase()
                            .trim_end_matches('.')
                            .ends_with("._oscjson._tcp.local")
                    {
                        let port = unsafe { record.Data.SRV.wPort };
                        if record.dwTtl == 0 {
                            ports.remove(&port);
                        } else if port > 0 && (ports.len() < 16 || ports.contains_key(&port)) {
                            ports.insert(
                                port,
                                now + Duration::from_secs(record.dwTtl.min(600) as u64),
                            );
                        }
                    }
                }
                row = record.pNext;
            }
        }
    } else if !cancelled {
        error(&browse.info, "browse", status);
    }
    if !records.is_null() {
        unsafe { DnsFree(Some(records.cast::<c_void>()), DnsFreeRecordList) };
    }
}

pub(crate) struct Discovery {
    browse: Arc<Browse>,
}
impl Discovery {
    pub(crate) fn new(candidates: Candidates, info: Info) -> Result<Self> {
        retain_callback_module()?;
        let query: Vec<u16> = "_oscjson._tcp.local"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut browse = Arc::new(Browse {
            request: DNS_SERVICE_BROWSE_REQUEST {
                Version: 1,
                InterfaceIndex: 0,
                QueryName: PCWSTR(query.as_ptr()),
                Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 {
                    pBrowseCallback: Some(browsed),
                },
                pQueryContext: ptr::null_mut(),
            },
            cancel: UnsafeCell::new(DNS_SERVICE_CANCEL::default()),
            _query_name: query,
            candidates,
            info,
        });
        let context = Arc::as_ptr(&browse).cast_mut().cast();
        Arc::get_mut(&mut browse).unwrap().request.pQueryContext = context;
        let pending = Arc::into_raw(browse.clone());
        let code = unsafe { DnsServiceBrowse(&browse.request, browse.cancel.get()) } as u32;
        if code != PENDING {
            unsafe { drop(Arc::from_raw(pending)) };
            anyhow::bail!("Windows DNS-SDの検出を開始できません: {code}");
        }
        Ok(Self { browse })
    }
}
impl Drop for Discovery {
    fn drop(&mut self) {
        let code = unsafe { DnsServiceBrowseCancel(self.browse.cancel.get()) } as u32;
        if code != 0 {
            error(&self.browse.info, "cancel", code);
        }
        // The final cancellation callback releases the raw subscription Arc.
        // No thread needs to block on DNSAPI while the user disables input.
    }
}
