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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::core::GUID;
use windows_sys::Win32::System::Diagnostics::Etw::*;

const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);
const D3D9: GUID = GUID::from_u128(0x783ACA0A_790E_4D7F_8451_AA850511C6B9);
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);
const DXGKRNL_KW_PRESENT: u64 = 0x0800_0000;

/// Session used by the overlay itself.
pub const SESSION: &str = "RustFpsOverlaySession";
/// Separate session for `--probe`, so probing never kills a running overlay's session.
pub const PROBE_SESSION: &str = "RustFpsOverlayProbe";

/// 100ns ticks (FILETIME units).
const SEC: i64 = 10_000_000;
/// How much history to keep per process.
const KEEP: i64 = 3 * SEC;
/// ETW real-time delivery lags up to ~1s (flush timer); no events for this long = "not presenting".
/// Measured with the monotonic arrival time of events, not their (wall-clock converted)
/// timestamps, so system clock changes cannot make every process look stale.
const STALE: Duration = Duration::from_millis(2500);

/// Present timestamps of one swap chain.
struct Chain {
    q: VecDeque<i64>,
    seen: Instant,
}

#[derive(Default)]
struct ProcFrames {
    /// API-level presents, one queue per swap chain (key = swap chain pointer): a process
    /// with several swap chains must not add their frame rates together.
    api: HashMap<u64, Chain>,
    kernel: VecDeque<i64>,
    kernel_seen: Option<Instant>,
}

impl ProcFrames {
    /// Timestamps of the busiest swap chain that is still presenting, else the kernel stream.
    fn active(&self) -> Option<&VecDeque<i64>> {
        let best = self
            .api
            .values()
            .filter(|c| c.seen.elapsed() < STALE)
            .max_by_key(|c| c.q.len());
        if let Some(c) = best {
            return Some(&c.q);
        }
        if self.kernel_seen.is_some_and(|t| t.elapsed() < STALE) {
            Some(&self.kernel)
        } else {
            None
        }
    }

    fn last_seen(&self) -> Option<Instant> {
        self.api.values().map(|c| c.seen).max().max(self.kernel_seen)
    }
}

/// Generation counter for consumer threads.
static NEXT_GEN: AtomicU32 = AtomicU32::new(0);
/// Generation of the live consumer thread, 0 = none. A thread only clears its own
/// generation (compare-exchange), so an old thread can never mark a new one dead.
static LIVE_GEN: AtomicU32 = AtomicU32::new(0);

/// False if the session died (e.g. stopped by someone else) and needs `start()` again.
pub fn running() -> bool {
    LIVE_GEN.load(Ordering::SeqCst) != 0
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

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let h = &(*rec).EventHeader;
    let id = h.EventDescriptor.Id;
    let p = &h.ProviderId;
    let (kernel, dxgi) = if guid_eq(p, &DXGI) {
        if id != 42 {
            return;
        }
        (false, true)
    } else if guid_eq(p, &D3D9) {
        if id != 1 {
            return;
        }
        (false, false)
    } else if guid_eq(p, &DXGKRNL) {
        if id != 184 {
            return;
        }
        (true, false)
    } else {
        return;
    };

    // DXGI/D3D9 Present_Start payload: swap chain pointer, then (DXGI) Flags: u32.
    let ud = (*rec).UserData as *const u8;
    let len = (*rec).UserDataLength as usize;
    let psz = if h.Flags & 0x20 != 0 { 4 } else { 8 }; // EVENT_HEADER_FLAG_32_BIT_HEADER
    let (mut chain, mut flags) = (0u64, 0u32);
    if !kernel && !ud.is_null() && len >= psz {
        chain = if psz == 8 {
            std::ptr::read_unaligned(ud as *const u64)
        } else {
            std::ptr::read_unaligned(ud as *const u32) as u64
        };
        if dxgi && len >= psz + 4 {
            flags = std::ptr::read_unaligned(ud.add(psz) as *const u32);
        }
    }
    // DXGI_PRESENT_TEST (0x1) probes the swap chain and presents nothing.
    if flags & 1 != 0 {
        return;
    }

    let ts = h.TimeStamp;
    let mut map = frames();
    let pf = map.entry(h.ProcessId).or_default();
    let q = if kernel {
        pf.kernel_seen = Some(Instant::now());
        &mut pf.kernel
    } else {
        if pf.api.len() >= 16 && !pf.api.contains_key(&chain) {
            // Bound the memory of a process that keeps creating swap chains.
            pf.api.retain(|_, c| c.seen.elapsed() < STALE);
        }
        let c = pf.api.entry(chain).or_insert_with(|| Chain { q: VecDeque::new(), seen: Instant::now() });
        c.seen = Instant::now();
        &mut c.q
    };
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

/// ETW session names are machine-wide: add the Windows session id so that two users
/// logged in at the same time (fast user switching) don't stop each other's session.
fn full_name(base: &str) -> String {
    let mut id = 0u32;
    unsafe {
        windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(
            windows_sys::Win32::System::Threading::GetCurrentProcessId(),
            &mut id,
        );
    }
    format!("{base}-{id}")
}

/// Stop our session (also used to clean a stale one left by a killed instance).
pub fn stop(session: &str) {
    let name = wide(&full_name(session));
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
pub fn start(session: &str) -> Result<(), u32> {
    stop(session);
    let full = full_name(session);
    let name = wide(&full);
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
        // One provider failing is tolerated (the others still give frames); all failing is fatal.
        let (mut enabled, mut last_err) = (0, 0u32);
        for (guid, kw) in [(DXGI, 0u64), (D3D9, 0u64), (DXGKRNL, DXGKRNL_KW_PRESENT)] {
            let r = EnableTraceEx2(
                handle,
                &guid,
                1, // EVENT_CONTROL_CODE_ENABLE_PROVIDER
                4, // TRACE_LEVEL_INFORMATION
                kw,
                0,
                0,
                null(),
            );
            if r == 0 {
                enabled += 1;
            } else {
                last_err = r;
            }
        }
        if enabled == 0 {
            stop(session);
            return Err(last_err);
        }
    }

    let my_gen = NEXT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    LIVE_GEN.store(my_gen, Ordering::SeqCst);
    std::thread::spawn(move || unsafe {
        let mut name = wide(&full);
        let mut lf: EVENT_TRACE_LOGFILEW = zeroed();
        lf.LoggerName = name.as_mut_ptr();
        // PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD
        lf.Anonymous1.ProcessTraceMode = 0x0000_0100 | 0x1000_0000;
        lf.Anonymous2.EventRecordCallback = Some(on_event);
        let h = OpenTraceW(&mut lf);
        // On OpenTrace failure the flag is cleared below and the app restarts the session.
        if std::mem::transmute::<PROCESSTRACE_HANDLE, u64>(h) != u64::MAX {
            ProcessTrace(&h, 1, null(), null()); // blocks until the session stops
            CloseTrace(h);
        }
        let _ = LIVE_GEN.compare_exchange(my_gen, 0, Ordering::SeqCst, Ordering::SeqCst);
    });
    Ok(())
}

/// Drop processes that stopped presenting.
pub fn prune() {
    const GONE: Duration = Duration::from_secs(3);
    frames().retain(|_, pf| {
        pf.api.retain(|_, c| c.seen.elapsed() < GONE);
        pf.last_seen().is_some_and(|t| t.elapsed() < GONE)
    });
}

/// All processes currently presenting frames.
pub fn active_pids() -> Vec<u32> {
    frames()
        .iter()
        .filter(|(_, pf)| pf.active().is_some())
        .map(|(&pid, _)| pid)
        .collect()
}

/// Frame statistics over the last second of presents for `pid`.
pub fn stats(pid: u32) -> Option<Stats> {
    let map = frames();
    let q = map.get(&pid)?.active()?;
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
