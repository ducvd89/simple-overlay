//! Real-time ETW session that records Present() timestamps per process.
//!
//! Sources (same idea as PresentMon, much simpler):
//!   * Microsoft-Windows-DXGI    Present_Start (id 42)  - D3D10/11/12
//!   * Microsoft-Windows-D3D9    Present_Start (id 1)   - D3D9
//!   * Microsoft-Windows-DxgKrnl Present_Info  (id 184) - kernel fallback, covers Vulkan/OpenGL
//! For each process the API-level stream is preferred; the kernel stream is
//! only used when the process emits no API-level presents.

use std::collections::{HashMap, VecDeque};
use std::mem::{size_of, zeroed};
use std::ptr::null;
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
const D3D9: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);
const DXGKRNL_KW_PRESENT: u64 = 0x0800_0000;

const SESSION: &str = "RustFpsOverlaySession";

/// 100ns ticks (FILETIME units).
const SEC: i64 = 10_000_000;
/// How much history to keep per process.
const KEEP: i64 = 3 * SEC;
/// ETW real-time delivery lags up to ~1s (flush timer); anything older than this is "not presenting".
const STALE: i64 = 2 * SEC + SEC / 2;

#[derive(Default)]
struct ProcFrames {
    api: VecDeque<i64>,
    kernel: VecDeque<i64>,
}

impl ProcFrames {
    fn active(&self, now: i64) -> Option<&VecDeque<i64>> {
        let fresh = |q: &VecDeque<i64>| q.back().is_some_and(|&t| now - t < STALE);
        if fresh(&self.api) {
            Some(&self.api)
        } else if fresh(&self.kernel) {
            Some(&self.kernel)
        } else {
            None
        }
    }
}

pub struct Stats {
    pub fps: f64,
    pub frametime_ms: f64,
    /// Most recent frame intervals in ms (oldest first), for the graph.
    pub intervals: Vec<f32>,
}

static FRAMES: OnceLock<Mutex<HashMap<u32, ProcFrames>>> = OnceLock::new();

fn frames() -> MutexGuard<'static, HashMap<u32, ProcFrames>> {
    FRAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Current time in the same clock as converted ETW timestamps (UTC FILETIME).
pub fn now() -> i64 {
    unsafe {
        let mut ft: FILETIME = zeroed();
        GetSystemTimePreciseAsFileTime(&mut ft);
        ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
    }
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let h = &(*rec).EventHeader;
    let id = h.EventDescriptor.Id;
    let p = &h.ProviderId;
    let kernel = if guid_eq(p, &DXGI) {
        if id != 42 {
            return;
        }
        false
    } else if guid_eq(p, &D3D9) {
        if id != 1 {
            return;
        }
        false
    } else if guid_eq(p, &DXGKRNL) {
        if id != 184 {
            return;
        }
        true
    } else {
        return;
    };

    let ts = h.TimeStamp;
    let mut map = frames();
    let pf = map.entry(h.ProcessId).or_default();
    let q = if kernel { &mut pf.kernel } else { &mut pf.api };
    q.push_back(ts);
    while q.front().is_some_and(|&f| ts - f > KEEP) {
        q.pop_front();
    }
}

/// EVENT_TRACE_PROPERTIES followed by room for the logger name, 8-byte aligned.
fn make_props(name_len: usize) -> Vec<u64> {
    let bytes = size_of::<EVENT_TRACE_PROPERTIES>() + name_len * 2 + 16;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    unsafe {
        let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*p).Wnode.BufferSize = (buf.len() * 8) as u32;
        (*p).Wnode.Flags = 0x0002_0000; // WNODE_FLAG_TRACED_GUID
        (*p).Wnode.ClientContext = 1; // QPC timestamps (high resolution)
        (*p).LogFileMode = 0x0000_0100; // EVENT_TRACE_REAL_TIME_MODE
        (*p).FlushTimer = 1; // seconds
        (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    }
    buf
}

/// Stop our session (also used to clean a stale one left by a killed instance).
pub fn stop() {
    let name = wide(SESSION);
    let mut props = make_props(name.len());
    unsafe {
        ControlTraceW(
            zeroed(),
            name.as_ptr(),
            props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
            1, // EVENT_TRACE_CONTROL_STOP
        );
    }
}

/// Start the session and a consumer thread. Returns the Win32 error code on failure
/// (5 = ERROR_ACCESS_DENIED -> needs admin / Performance Log Users).
pub fn start() -> Result<(), u32> {
    stop();
    let name = wide(SESSION);
    let mut props = make_props(name.len());
    unsafe {
        let mut handle: CONTROLTRACE_HANDLE = zeroed();
        let r = StartTraceW(
            &mut handle,
            name.as_ptr(),
            props.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES,
        );
        if r != 0 {
            return Err(r);
        }
        for (guid, kw) in [(DXGI, 0u64), (D3D9, 0u64), (DXGKRNL, DXGKRNL_KW_PRESENT)] {
            EnableTraceEx2(
                handle,
                &guid,
                1, // EVENT_CONTROL_CODE_ENABLE_PROVIDER
                4, // TRACE_LEVEL_INFORMATION
                kw,
                0,
                0,
                null(),
            );
        }
    }

    std::thread::spawn(|| unsafe {
        let mut name = wide(SESSION);
        let mut lf: EVENT_TRACE_LOGFILEW = zeroed();
        lf.LoggerName = name.as_mut_ptr();
        // PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD
        lf.Anonymous1.ProcessTraceMode = 0x0000_0100 | 0x1000_0000;
        lf.Anonymous2.EventRecordCallback = Some(on_event);
        let h = OpenTraceW(&mut lf);
        if std::mem::transmute::<PROCESSTRACE_HANDLE, u64>(h) == u64::MAX {
            return;
        }
        ProcessTrace(&h, 1, null(), null()); // blocks until the session stops
        CloseTrace(h);
    });
    Ok(())
}

/// Drop processes that stopped presenting.
pub fn prune(now: i64) {
    frames().retain(|_, pf| {
        let last = pf.api.back().max(pf.kernel.back()).copied().unwrap_or(0);
        now - last < KEEP
    });
}

/// All processes currently presenting frames.
pub fn active_pids(now: i64) -> Vec<u32> {
    frames()
        .iter()
        .filter(|(_, pf)| pf.active(now).is_some())
        .map(|(&pid, _)| pid)
        .collect()
}

/// Frame statistics over the last second of presents for `pid`.
pub fn stats(pid: u32, now: i64) -> Option<Stats> {
    let map = frames();
    let q = map.get(&pid)?.active(now)?;
    let last = *q.back()?;
    let window: Vec<i64> = q.iter().copied().filter(|&t| last - t <= SEC).collect();
    if window.len() < 2 {
        return Some(Stats { fps: 0.0, frametime_ms: 0.0, intervals: Vec::new() });
    }
    let span = (window[window.len() - 1] - window[0]) as f64;
    let fps = (window.len() - 1) as f64 * SEC as f64 / span.max(1.0);

    let all: Vec<i64> = q.iter().copied().collect();
    let intervals: Vec<f32> = all
        .windows(2)
        .rev()
        .take(120)
        .map(|w| (w[1] - w[0]) as f32 / 10_000.0)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Some(Stats { fps, frametime_ms: 1000.0 / fps, intervals })
}
