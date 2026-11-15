//! Temperature sensors that work without a kernel driver.
//!   * GPU: NVIDIA NVML (nvml.dll ships with the driver).
//!   * CPU: ACPI thermal zones via PDH ("Thermal Zone Information" counters).
//!     On most laptops the hottest zone tracks the CPU package; desktops may
//!     not expose a useful zone, in which case "--" is shown.

use std::ffi::c_void;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::{null, null_mut};

use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Performance::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- NVML

type NvmlInit = unsafe extern "C" fn() -> i32;
type NvmlShutdown = unsafe extern "C" fn() -> i32;
type NvmlGetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvmlGetTemp = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
type NvmlGetTempV = unsafe extern "C" fn(*mut c_void, *mut NvmlTemperature) -> i32;

/// nvmlTemperature_v1_t
#[repr(C)]
struct NvmlTemperature {
    version: u32,
    sensor_type: i32, // 0 = NVML_TEMPERATURE_GPU
    temperature: i32,
}
/// nvmlTemperature_v1: sizeof(struct) | (1 << 24)
const NVML_TEMPERATURE_V1: u32 = size_of::<NvmlTemperature>() as u32 | (1 << 24);

/// Readings outside this range are treated as driver glitches.
const TEMP_RANGE: std::ops::RangeInclusive<i64> = 1..=125;
/// How many consecutive bad readings the last good value is held for.
const HOLD_TICKS: u32 = 4;
/// After this many consecutive failures the caller should re-initialise NVML.
const RELOAD_AFTER: u32 = 10;

/// Bad-reading log lines written by this process (the log must stay bounded even when
/// NVML is reloaded over and over).
static LOGGED: AtomicU32 = AtomicU32::new(0);
const LOG_LIMIT: u32 = 20;

pub struct Nvml {
    lib: HMODULE,
    dev: *mut c_void,
    shutdown: Option<NvmlShutdown>,
    get_temp: NvmlGetTemp,
    /// Newer API (the old one is deprecated); preferred when exported.
    get_temp_v: Option<NvmlGetTempV>,
    last_good: Option<f32>,
    bad_streak: u32,
}

impl Nvml {
    pub fn load() -> Option<Self> {
        unsafe {
            // Never search the exe directory: this process may be elevated.
            let mut lib =
                LoadLibraryExW(wide("nvml.dll").as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
            if lib.is_null() {
                // Absolute path, and its dependencies come from its own folder / System32 only.
                lib = LoadLibraryExW(
                    wide(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll").as_ptr(),
                    null_mut(),
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
                );
            }
            if lib.is_null() {
                return None;
            }
            let r = Self::init_from(lib);
            if r.is_none() {
                FreeLibrary(lib);
            }
            r
        }
    }

    unsafe fn init_from(lib: HMODULE) -> Option<Self> {
        {
            let init: NvmlInit = transmute(GetProcAddress(lib, b"nvmlInit_v2\0".as_ptr())?);
            let shutdown: Option<NvmlShutdown> =
                GetProcAddress(lib, b"nvmlShutdown\0".as_ptr()).map(|f| transmute(f));
            let get_handle: NvmlGetHandle =
                transmute(GetProcAddress(lib, b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr())?);
            let get_temp: NvmlGetTemp =
                transmute(GetProcAddress(lib, b"nvmlDeviceGetTemperature\0".as_ptr())?);
            let get_temp_v: Option<NvmlGetTempV> =
                GetProcAddress(lib, b"nvmlDeviceGetTemperatureV\0".as_ptr()).map(|f| transmute(f));
            if init() != 0 {
                return None;
            }
            let mut dev = null_mut();
            if get_handle(0, &mut dev) != 0 {
                if let Some(sd) = shutdown {
                    sd();
                }
                return None;
            }
            Some(Self { lib, dev, shutdown, get_temp, get_temp_v, last_good: None, bad_streak: 0 })
        }
    }

    /// Raw reading: (nvml return code, temperature).
    fn read_raw(&self) -> (i32, i64) {
        unsafe {
            if let Some(f) = self.get_temp_v {
                let mut t = NvmlTemperature { version: NVML_TEMPERATURE_V1, sensor_type: 0, temperature: 0 };
                let rc = f(self.dev, &mut t);
                if rc == 0 {
                    return (0, t.temperature as i64);
                }
                // Older drivers: fall through to the legacy call.
            }
            let mut t = 0u32;
            let rc = (self.get_temp)(self.dev, 0, &mut t);
            (rc, t as i64)
        }
    }

    /// GPU temperature in °C. Implausible readings are logged and replaced by the
    /// last good value for a few ticks, then by `None`.
    pub fn temp(&mut self) -> Option<f32> {
        let (rc, raw) = self.read_raw();
        if rc == 0 && TEMP_RANGE.contains(&raw) {
            self.bad_streak = 0;
            self.last_good = Some(raw as f32);
            return self.last_good;
        }
        self.bad_streak += 1;
        if LOGGED.fetch_add(1, Ordering::Relaxed) < LOG_LIMIT {
            log_line(&format!("nvml bad reading: rc={rc} raw={raw}"));
        }
        if self.bad_streak <= HOLD_TICKS {
            self.last_good
        } else {
            None
        }
    }

    /// True when NVML keeps failing and should be shut down and loaded again.
    pub fn needs_reload(&self) -> bool {
        self.bad_streak >= RELOAD_AFTER
    }

    /// Readings are currently good (used to reset the reload back-off).
    pub fn healthy(&self) -> bool {
        self.bad_streak == 0 && self.last_good.is_some()
    }

    /// Forget the held value (call when no game is running, so a stale
    /// temperature from an earlier session is never shown).
    pub fn reset(&mut self) {
        self.last_good = None;
        self.bad_streak = 0;
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            if let Some(sd) = self.shutdown {
                sd();
            }
            FreeLibrary(self.lib);
        }
    }
}

/// Diagnostics for rare driver glitches. This process usually runs elevated, so it must
/// never append to a file in a user-writable folder (a junction could redirect the write):
/// the log goes to `%ProgramFiles%\SimpleOverlay\overlay.log` when that folder exists and
/// is writable (admin only), otherwise to the debugger (OutputDebugString / DebugView).
fn log_line(msg: &str) {
    use std::io::Write;
    if let Some(dir) = crate::tray::install_dir().filter(|d| d.is_dir()) {
        if let Ok(mut f) =
            std::fs::OpenOptions::new().create(true).append(true).open(dir.join("overlay.log"))
        {
            if writeln!(f, "{msg}").is_ok() {
                return;
            }
        }
    }
    unsafe { windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW(wide(msg).as_ptr()) };
}

// ---------------------------------------------------------------- Thermal zones

pub struct Thermal {
    query: isize,
    counter: isize,
    /// "High Precision Temperature" is in tenths of Kelvin, "Temperature" in Kelvin.
    tenths: bool,
}

impl Thermal {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut query: isize = zeroed();
            if PdhOpenQueryW(null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter: isize = zeroed();
            let mut tenths = true;
            let hp = wide(r"\Thermal Zone Information(*)\High Precision Temperature");
            if PdhAddEnglishCounterW(query, hp.as_ptr(), 0, &mut counter) != 0 {
                tenths = false;
                let lp = wide(r"\Thermal Zone Information(*)\Temperature");
                if PdhAddEnglishCounterW(query, lp.as_ptr(), 0, &mut counter) != 0 {
                    PdhCloseQuery(query);
                    return None;
                }
            }
            PdhCollectQueryData(query);
            Some(Self { query, counter, tenths })
        }
    }

    /// Hottest thermal zone in °C.
    pub fn read(&mut self) -> Option<f32> {
        const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
        const PDH_MORE_DATA: u32 = 0x8000_07D2;
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                null_mut(),
            );
            if r != PDH_MORE_DATA || size == 0 {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                items,
            ) != 0
            {
                return None;
            }
            (0..count as usize)
                .map(|i| &*items.add(i))
                .filter(|it| it.FmtValue.CStatus <= 1) // VALID_DATA / NEW_DATA
                .map(|it| {
                    let v = it.FmtValue.Anonymous.doubleValue;
                    let k = if self.tenths { v / 10.0 } else { v };
                    (k - 273.15) as f32
                })
                .filter(|c| (1.0..125.0).contains(c))
                .reduce(f32::max)
        }
    }
}

impl Drop for Thermal {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}
