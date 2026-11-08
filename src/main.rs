#![windows_subsystem = "windows"]

mod etw;
mod sensors;
mod tray;

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ------------------------------------------------------------------ config

const TICK_MS: u32 = 500;
const ALPHA: u8 = 225;
const BASE_W: i32 = 170;
const PAD: i32 = 8;
const MARGIN: i32 = 0;
const HOTKEY_TOGGLE: i32 = 1; // Ctrl+Shift+F11
const HOTKEY_QUIT: i32 = 2; // Ctrl+Shift+F12

/// Processes that present frames but are never "the game".
const IGNORE: &[&str] = &[
    "dwm.exe", "explorer.exe", "searchhost.exe", "startmenuexperiencehost.exe",
    "shellexperiencehost.exe", "textinputhost.exe", "applicationframehost.exe",
    "lockapp.exe", "systemsettings.exe", "taskmgr.exe", "chrome.exe", "msedge.exe",
    "msedgewebview2.exe", "firefox.exe", "opera.exe", "brave.exe", "vivaldi.exe",
    "discord.exe", "steamwebhelper.exe", "epicwebhelper.exe", "code.exe", "claude.exe",
    "spotify.exe", "obs64.exe", "windowsterminal.exe", "vlc.exe", "mpc-hc64.exe",
    "nvidia app.exe", "nvcontainer.exe", "slack.exe", "teams.exe", "ms-teams.exe",
];

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
const BG: (u8, u8, u8) = (14, 15, 20);
const DIM: (u8, u8, u8) = (150, 155, 170);
const TEXT: (u8, u8, u8) = (235, 238, 245);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ------------------------------------------------------------------ state

struct Game {
    pid: u32,
    #[allow(dead_code)] // kept for future display / logging
    name: String,
}

struct Fonts {
    scale: f32,
    label: HFONT,
    value: HFONT,
}

struct App {
    hwnd: HWND,
    game: Option<Game>,
    stats: Option<etw::Stats>,
    cpu: Option<f32>,
    gpu: Option<f32>,
    nvml: Option<Option<sensors::Nvml>>, // None = not tried yet
    thermal: Option<sensors::Thermal>,
    names: HashMap<u32, ProcInfo>,
    ignore: Vec<String>,
    hidden: bool,
    show_clock: bool,
    icon: HICON,
    splash_until: Instant,
    etw_error: Option<u32>,
    scale: f32,
    fonts: Option<Fonts>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.borrow_mut().as_mut().map(f))
}

// ------------------------------------------------------------------ process helpers

fn process_name(pid: u32) -> Option<String> {
    process_path(pid).and_then(|p| p.rsplit('\\').next().map(str::to_owned))
}

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Built-in ignore list + names from `ignore.txt` next to the exe (one exe name per line).
fn load_ignore_list() -> Vec<String> {
    let mut list: Vec<String> = IGNORE.iter().map(|s| s.to_string()).collect();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_owned())) {
        if let Ok(txt) = std::fs::read_to_string(dir.join("ignore.txt")) {
            list.extend(
                txt.lines()
                    .map(|l| l.trim().to_ascii_lowercase())
                    .filter(|l| !l.is_empty() && !l.starts_with('#')),
            );
        }
    }
    list
}

struct ProcInfo {
    name: String,
    ignored: bool,
}

fn proc_info(pid: u32, hwnd: HWND, ignore: &[String]) -> ProcInfo {
    match process_path(pid) {
        Some(path) => {
            let name = path.rsplit('\\').next().unwrap_or(&path).to_owned();
            let lower = path.to_ascii_lowercase();
            let system = lower.starts_with(r"c:\windows\");
            let ignored = system || ignore.iter().any(|i| *i == name.to_ascii_lowercase());
            ProcInfo { name, ignored }
        }
        // Protected processes (e.g. some anti-cheat) may deny the query: fall back to the title.
        None => ProcInfo { name: window_title(hwnd), ignored: false },
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn monitor_of(hwnd: HWND) -> (RECT, f32) {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (mi.rcMonitor, dx as f32 / 96.0)
    }
}

/// A window is "game-like" if it covers most of its monitor (fullscreen/borderless
/// or a large windowed game).
fn covers_monitor(hwnd: HWND) -> bool {
    unsafe {
        let mut r: RECT = zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return false;
        }
        let (m, _) = monitor_of(hwnd);
        let area = |r: &RECT| ((r.right - r.left).max(0) as i64) * ((r.bottom - r.top).max(0) as i64);
        area(&r) * 2 >= area(&m)
    }
}

// ------------------------------------------------------------------ tick

fn tick(app: &mut App) {
    let now = etw::now();
    etw::prune(now);

    let own = unsafe { GetCurrentProcessId() };
    let fg = unsafe { GetForegroundWindow() };
    let mut fg_pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, &mut fg_pid) };

    // Detection: the foreground process is the game if it is presenting frames,
    // is not on the ignore list, and either was already detected (sticky, so
    // loading screens with low fps keep it) or looks like a game (big window,
    // steady frame rate).
    let mut detected = None;
    if fg_pid != 0 && fg_pid != own {
        let ignore = &app.ignore;
        let info = app.names.entry(fg_pid).or_insert_with(|| proc_info(fg_pid, fg, ignore));
        let name = info.name.clone();
        if !info.ignored {
            if let Some(st) = etw::stats(fg_pid, now) {
                let sticky = app.game.as_ref().is_some_and(|g| g.pid == fg_pid);
                if sticky || (st.fps >= 15.0 && covers_monitor(fg)) {
                    detected = Some((Game { pid: fg_pid, name }, st));
                }
            }
        }
    }
    if app.names.len() > 512 {
        app.names.clear();
    }

    match detected {
        Some((game, st)) => {
            app.stats = Some(st);
            app.game = Some(game);
            // NVML is loaded lazily so an idle Optimus dGPU is not woken up needlessly.
            if app.nvml.is_none() {
                app.nvml = Some(sensors::Nvml::load());
            }
            app.gpu = app.nvml.as_ref().and_then(|n| n.as_ref()).and_then(|n| n.temp());
        }
        None => {
            app.game = None;
            app.stats = None;
            app.gpu = None;
        }
    }
    app.cpu = app.thermal.as_mut().and_then(|t| t.read());
}

/// Line height and chart width at 96 DPI.
const LINE_H: i32 = 20;
const CHART_W: i32 = 40;

/// Computes visibility/placement after a tick (no window calls while APP is borrowed).
fn placement(app: &mut App) -> Option<(i32, i32, i32, i32)> {
    let splash = Instant::now() < app.splash_until;
    let show = !app.hidden && (app.game.is_some() || splash || app.etw_error.is_some());
    if !show {
        return None;
    }
    let fg = unsafe { GetForegroundWindow() };
    let (mon, scale) = monitor_of(if app.game.is_some() { fg } else { null_mut() });
    app.scale = scale;
    let s = |v: i32| (v as f32 * scale).round() as i32;
    let h = s(LINE_H);
    // Measure the line with an off-screen DC so the window hugs its content.
    let w = unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let w = draw_line(app, mem, h, false);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        w
    };
    Some((mon.left + s(MARGIN), mon.top + s(MARGIN), w, h))
}

fn apply_placement(hwnd: HWND, p: Option<(i32, i32, i32, i32)>) {
    unsafe {
        match p {
            None => {
                ShowWindow(hwnd, SW_HIDE);
            }
            Some((x, y, w, h)) => {
                // Flush against the screen corner: square edges, no rounded region.
                // Re-assert topmost every tick: games and other overlays fight for it.
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
                InvalidateRect(hwnd, null(), 0);
            }
        }
    }
}

// ------------------------------------------------------------------ painting

unsafe fn make_font(px: f32, weight: i32, face: &str) -> HFONT {
    CreateFontW(
        -(px.round() as i32), 0, 0, 0, weight, 0, 0, 0,
        1, // DEFAULT_CHARSET
        0, 0,
        5, // CLEARTYPE_QUALITY
        0,
        wide(face).as_ptr(),
    )
}

fn fonts(app: &mut App) -> (HFONT, HFONT) {
    let scale = app.scale;
    if app.fonts.as_ref().is_none_or(|f| f.scale != scale) {
        if let Some(old) = app.fonts.take() {
            unsafe {
                DeleteObject(old.label);
                DeleteObject(old.value);
            }
        }
        unsafe {
            app.fonts = Some(Fonts {
                scale,
                label: make_font(10.0 * scale, 600, "Segoe UI"),
                value: make_font(12.5 * scale, 700, "Segoe UI"),
            });
        }
    }
    let f = app.fonts.as_ref().unwrap();
    (f.label, f.value)
}

unsafe fn text_width(hdc: HDC, font: HFONT, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    SelectObject(hdc, font);
    let mut sz: SIZE = zeroed();
    GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut sz);
    sz.cx
}

/// (ascent, height) of a font.
unsafe fn metrics(hdc: HDC, font: HFONT) -> (i32, i32) {
    SelectObject(hdc, font);
    let mut tm: TEXTMETRICW = zeroed();
    GetTextMetricsW(hdc, &mut tm);
    (tm.tmAscent, tm.tmHeight)
}

fn fps_color(fps: f64) -> (u8, u8, u8) {
    match fps {
        f if f >= 55.0 => (110, 231, 140),
        f if f >= 30.0 => (250, 204, 21),
        _ => (248, 113, 113),
    }
}

fn temp_color(t: f32) -> (u8, u8, u8) {
    match t {
        t if t >= 90.0 => (248, 113, 113),
        t if t >= 80.0 => (250, 204, 21),
        _ => TEXT,
    }
}

fn fmt_temp(t: Option<f32>) -> String {
    t.map_or("--".into(), |t| format!("{:.0}°", t))
}

/// Lays out (and optionally draws) the single overlay line. Returns its total width.
///   120 FPS  8.3 ms  [chart]  CPU 77°  GPU 75°
unsafe fn draw_line(app: &mut App, hdc: HDC, h: i32, draw: bool) -> i32 {
    let scale = app.scale;
    let s = |v: i32| (v as f32 * scale).round() as i32;
    let (label, value) = fonts(app);
    SetBkMode(hdc, TRANSPARENT as _);

    // Shared baseline so labels and values of different sizes line up.
    let (va, vh) = metrics(hdc, value);
    let (la, _) = metrics(hdc, label);
    let baseline = (h - vh) / 2 + va;

    let x = std::cell::Cell::new(s(PAD + 2));
    // Draw `txt` right-aligned in a slot as wide as `slot` (keeps numbers from jittering).
    let put = |font: HFONT, color: (u8, u8, u8), txt: &str, slot: &str, gap: i32| {
        let tw = text_width(hdc, font, txt);
        let sw = text_width(hdc, font, slot).max(tw);
        if draw {
            let asc = if font == label { la } else { va };
            let w: Vec<u16> = txt.encode_utf16().collect();
            SelectObject(hdc, font);
            SetTextColor(hdc, rgb(color.0, color.1, color.2));
            TextOutW(hdc, x.get() + sw - tw, baseline - asc, w.as_ptr(), w.len() as i32);
        }
        x.set(x.get() + sw + gap);
    };

    let Some(_) = &app.game else {
        let msg = match app.etw_error {
            Some(5) => "FPS overlay: needs admin rights".to_string(),
            Some(e) => format!("FPS overlay: ETW error {e}"),
            None => "FPS overlay  ·  Ctrl+Shift+F11 hide  ·  F12 exit".to_string(),
        };
        put(label, TEXT, &msg, "", 0);
        return x.get() + s(PAD + 2);
    };

    let stats = app.stats.as_ref().filter(|st| st.fps > 0.0);
    let fps = stats.map_or(0.0, |st| st.fps);

    match stats {
        Some(st) => {
            put(value, fps_color(st.fps), &format!("{:.0}", st.fps), "000", s(3));
            put(label, DIM, "FPS", "", s(9));
            put(value, TEXT, &format!("{:.1}", st.frametime_ms), "00.0", s(2));
        }
        None => {
            put(value, DIM, "--", "000", s(3));
            put(label, DIM, "FPS", "", s(9));
            put(value, DIM, "--", "00.0", s(2));
        }
    }
    put(label, DIM, "ms", "", s(7));

    // Frametime chart
    let (cx, cw) = (x.get(), s(CHART_W));
    let (top, bottom) = (s(4), h - s(4));
    if draw {
        let bg = CreateSolidBrush(rgb(30, 32, 42));
        FillRect(hdc, &RECT { left: cx, top, right: cx + cw, bottom }, bg);
        DeleteObject(bg);
        if let Some(st) = stats {
            let iv = &st.intervals[st.intervals.len().saturating_sub(60)..];
            if iv.len() >= 2 {
                let avg = iv.iter().sum::<f32>() / iv.len() as f32;
                let max = iv.iter().copied().fold(avg * 2.0, f32::max).max(1.0);
                let gh = (bottom - top - 2) as f32;
                let pts: Vec<POINT> = iv
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| POINT {
                        x: cx + (i as f32 / (iv.len() - 1) as f32 * (cw - 1) as f32) as i32,
                        y: bottom - 1 - ((v / max) * gh) as i32,
                    })
                    .collect();
                let c = fps_color(fps);
                let pen = CreatePen(PS_SOLID, 1, rgb(c.0, c.1, c.2));
                let old = SelectObject(hdc, pen);
                Polyline(hdc, pts.as_ptr(), pts.len() as i32);
                SelectObject(hdc, old);
                DeleteObject(pen);
            }
        }
    }
    x.set(cx + cw + s(9));

    put(label, DIM, "CPU", "", s(3));
    put(value, app.cpu.map_or(DIM, temp_color), &fmt_temp(app.cpu), "00°", s(9));
    put(label, DIM, "GPU", "", s(3));
    let clock_gap = if app.show_clock { s(10) } else { 0 };
    put(value, app.gpu.map_or(DIM, temp_color), &fmt_temp(app.gpu), "00°", clock_gap);
    if app.show_clock {
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        put(value, TEXT, &format!("{:02}:{:02}", t.wHour, t.wMinute), "00:00", 0);
    }

    x.get() + s(PAD + 2)
}

unsafe fn paint(app: &mut App, hdc: HDC, w: i32, h: i32) {
    let bg = CreateSolidBrush(rgb(BG.0, BG.1, BG.2));
    FillRect(hdc, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
    DeleteObject(bg);
    draw_line(app, hdc, h, true);
}

// ------------------------------------------------------------------ window proc

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn toggle_hidden(hwnd: HWND) {
    let p = with_app(|a| {
        a.hidden = !a.hidden;
        placement(a)
    })
    .flatten();
    apply_placement(hwnd, p);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let p = with_app(|a| {
                tick(a);
                placement(a)
            })
            .flatten();
            apply_placement(hwnd, p);
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            let (w, h) = (r.right, r.bottom);
            // Double buffer to avoid flicker.
            let mem = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem, bmp);
            with_app(|a| paint(a, mem, w, h));
            BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
            EndPaint(hwnd, &ps);
            0
        }
        WM_HOTKEY => {
            match wp as i32 {
                HOTKEY_TOGGLE => toggle_hidden(hwnd),
                HOTKEY_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        tray::WM_TRAY => {
            match lp as u32 {
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let (visible, clock) = with_app(|a| (!a.hidden, a.show_clock)).unwrap_or((true, false));
                    match tray::show_menu(hwnd, visible, clock) {
                        tray::CMD_TOGGLE => toggle_hidden(hwnd),
                        tray::CMD_CLOCK => {
                            let p = with_app(|a| {
                                a.show_clock = !a.show_clock;
                                tray::set_flag(tray::SETTING_CLOCK, a.show_clock);
                                placement(a)
                            })
                            .flatten();
                            apply_placement(hwnd, p);
                        }
                        tray::CMD_AUTOSTART => tray::set_autostart(!tray::autostart_enabled()),
                        tray::CMD_QUIT => {
                            DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                }
                WM_LBUTTONDBLCLK => toggle_hidden(hwnd),
                _ => {}
            }
            0
        }
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_DESTROY => {
            tray::remove(hwnd);
            etw::stop();
            PostQuitMessage(0);
            0
        }
        _ => {
            // Explorer restarted: the tray icon is gone and must be re-added.
            if msg == TASKBAR_CREATED.load(Ordering::Relaxed) && msg != 0 {
                if let Some(icon) = with_app(|a| a.icon) {
                    tray::add(hwnd, icon);
                }
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

// ------------------------------------------------------------------ main

fn relaunch_as_admin() -> bool {
    unsafe {
        let mut buf = [0u16; 1024];
        let n = windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(
            null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        );
        if n == 0 {
            return false;
        }
        let r = ShellExecuteW(
            null_mut(),
            wide("runas").as_ptr(),
            buf.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        );
        r as isize > 32
    }
}

/// `overlay.exe --probe`: print presenting processes and sensor readings, then exit.
fn probe() {
    match etw::start() {
        Ok(()) => println!("ETW session: ok"),
        Err(e) => println!("ETW session: error {e}"),
    }
    let mut thermal = sensors::Thermal::new();
    std::thread::sleep(Duration::from_secs(4));
    let now = etw::now();
    for pid in etw::active_pids(now) {
        if let Some(st) = etw::stats(pid, now) {
            let name = process_name(pid).unwrap_or_else(|| "?".into());
            println!("{pid:>6}  {name:<32} {:>6.1} fps  {:>6.2} ms", st.fps, st.frametime_ms);
        }
    }
    println!("CPU (thermal zone): {}", fmt_temp(thermal.as_mut().and_then(|t| t.read())));
    println!("GPU (NVML): {}", fmt_temp(sensors::Nvml::load().and_then(|n| n.temp())));
    etw::stop();
}

fn main() {
    if std::env::args().any(|a| a == "--probe") {
        probe();
        return;
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Single instance.
        let mutex_name = wide("Local\\RustFpsOverlay.Instance");
        let mut mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // ETW needs admin (or Performance Log Users). Try first, elevate only if denied.
        let etw_error = match etw::start() {
            Ok(()) => None,
            Err(5) => {
                CloseHandle(mutex);
                if relaunch_as_admin() {
                    return;
                }
                mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
                Some(5)
            }
            Err(e) => Some(e),
        };
        let _keep_mutex = mutex;

        let hinst = GetModuleHandleW(null());
        let class = wide("RustFpsOverlayWnd");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide("FPS Overlay").as_ptr(),
            WS_POPUP,
            MARGIN,
            MARGIN,
            BASE_W,
            60,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            etw::stop();
            return;
        }
        SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);

        RegisterHotKey(hwnd, HOTKEY_TOGGLE, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, VK_F11 as u32);
        RegisterHotKey(hwnd, HOTKEY_QUIT, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, VK_F12 as u32);

        let icon = tray::make_icon();
        TASKBAR_CREATED.store(tray::taskbar_created_msg(), Ordering::Relaxed);
        tray::add(hwnd, icon);
        tray::refresh_autostart();

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                game: None,
                stats: None,
                cpu: None,
                gpu: None,
                nvml: None,
                thermal: sensors::Thermal::new(),
                names: HashMap::new(),
                ignore: load_ignore_list(),
                hidden: false,
                show_clock: tray::get_flag(tray::SETTING_CLOCK).unwrap_or(false),
                icon,
                splash_until: Instant::now() + Duration::from_secs(5),
                etw_error,
                scale: 1.0,
                fonts: None,
            })
        });

        let p = with_app(placement).flatten();
        apply_placement(hwnd, p);
        SetTimer(hwnd, 1, TICK_MS, None);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = with_app(|a| a.hwnd);
    }
}
