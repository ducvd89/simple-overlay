//! Temperature sensors that work without a kernel driver.
//!   * GPU: NVIDIA NVML (nvml.dll ships with the driver).
//!   * CPU: ACPI thermal zones via PDH ("Thermal Zone Information" counters).
//!     On most laptops the hottest zone tracks the CPU package; desktops may
//!     not expose a useful zone, in which case "--" is shown.

use std::ffi::c_void;
use std::mem::{transmute, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Performance::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- NVML

type NvmlInit = unsafe extern "C" fn() -> i32;
type NvmlGetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvmlGetTemp = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;

pub struct Nvml {
    dev: *mut c_void,
    get_temp: NvmlGetTemp,
}

impl Nvml {
    pub fn load() -> Option<Self> {
        unsafe {
            let mut lib = LoadLibraryW(wide("nvml.dll").as_ptr());
            if lib.is_null() {
                lib = LoadLibraryW(
                    wide(r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll").as_ptr(),
                );
            }
            if lib.is_null() {
                return None;
            }
            let init: NvmlInit = transmute(GetProcAddress(lib, b"nvmlInit_v2\0".as_ptr())?);
            let get_handle: NvmlGetHandle =
                transmute(GetProcAddress(lib, b"nvmlDeviceGetHandleByIndex_v2\0".as_ptr())?);
            let get_temp: NvmlGetTemp =
                transmute(GetProcAddress(lib, b"nvmlDeviceGetTemperature\0".as_ptr())?);
            if init() != 0 {
                return None;
            }
            let mut dev = null_mut();
            if get_handle(0, &mut dev) != 0 {
                return None;
            }
            Some(Self { dev, get_temp })
        }
    }

    pub fn temp(&self) -> Option<f32> {
        let mut t = 0u32;
        // 0 = NVML_TEMPERATURE_GPU
        (unsafe { (self.get_temp)(self.dev, 0, &mut t) } == 0).then_some(t as f32)
    }
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
