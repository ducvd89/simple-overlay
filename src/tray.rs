//! System tray icon, its context menu, and the "start with Windows" option.

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Callback message the tray icon sends to the overlay window.
pub const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;

pub const CMD_TOGGLE: usize = 100;
pub const CMD_AUTOSTART: usize = 101;
pub const CMD_QUIT: usize = 102;
pub const CMD_CLOCK: usize = 103;
pub const CMD_IGNORE: usize = 104;

const SETTINGS_KEY: &str = r"Software\FpsOverlay";
pub const SETTING_CLOCK: &str = "ShowClock";
pub const SETTING_IGNORED: &str = "IgnoredApps";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "FpsOverlay";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

// ------------------------------------------------------------------ icon

/// The icon embedded in the exe (resource id 1, from assets/icon.ico) at tray size.
pub fn load_icon() -> Option<HICON> {
    unsafe {
        let hinst = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null());
        let h = LoadImageW(
            hinst,
            1usize as *const u16, // MAKEINTRESOURCE(1)
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        (!h.is_null()).then_some(h)
    }
}

/// Fallback: draws a small rounded tile with a green frametime-style line.
pub fn make_icon() -> HICON {
    const N: i32 = 32;
    unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let color = CreateCompatibleBitmap(screen, N, N);
        let mask = CreateBitmap(N, N, 1, 1, null());

        // Mask: white = transparent, black = opaque (the rounded tile).
        let old = SelectObject(dc, mask);
        PatBlt(dc, 0, 0, N, N, WHITENESS);
        SelectObject(dc, GetStockObject(BLACK_BRUSH));
        SelectObject(dc, GetStockObject(BLACK_PEN));
        RoundRect(dc, 0, 0, N, N, 10, 10);

        // Color: black outside the tile (required for transparency), dark tile, green line.
        SelectObject(dc, color);
        PatBlt(dc, 0, 0, N, N, BLACKNESS);
        let tile = CreateSolidBrush(rgb(30, 33, 44));
        let old_brush = SelectObject(dc, tile);
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        RoundRect(dc, 0, 0, N + 1, N + 1, 10, 10);
        let pen = CreatePen(PS_SOLID, 3, rgb(110, 231, 140));
        SelectObject(dc, pen);
        let pts = [
            POINT { x: 5, y: 22 },
            POINT { x: 11, y: 16 },
            POINT { x: 16, y: 20 },
            POINT { x: 21, y: 9 },
            POINT { x: 27, y: 13 },
        ];
        Polyline(dc, pts.as_ptr(), pts.len() as i32);

        // Deselect our pen/brush before deleting them (otherwise they leak).
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        SelectObject(dc, old);
        DeleteObject(pen);
        DeleteObject(tile);
        DeleteDC(dc);
        ReleaseDC(null_mut(), screen);

        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask);
        DeleteObject(color);
        icon
    }
}

// ------------------------------------------------------------------ tray icon

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { zeroed() };
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = TRAY_ID;
    n
}

/// Adds the tray icon. Returns false if Explorer refused (e.g. the taskbar does not exist
/// yet when started at logon); the caller retries.
pub fn add(hwnd: HWND, icon: HICON) -> bool {
    let mut n = nid(hwnd);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = icon;
    let tip: Vec<u16> = "FPS Overlay".encode_utf16().collect();
    n.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe { Shell_NotifyIconW(NIM_ADD, &n) != 0 }
}

pub fn remove(hwnd: HWND) {
    let n = nid(hwnd);
    unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
}

/// Message Explorer broadcasts after it restarts; the icon must be re-added then.
pub fn taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }
}

/// Shows the context menu at the cursor and returns the chosen command (0 = none).
pub fn show_menu(hwnd: HWND, overlay_visible: bool, clock: bool) -> usize {
    unsafe {
        let menu = CreatePopupMenu();
        let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide("FPS Overlay").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(
            menu,
            MF_STRING | check(overlay_visible),
            CMD_TOGGLE,
            wide("Show overlay\tCtrl+Shift+F11").as_ptr(),
        );
        AppendMenuW(menu, MF_STRING | check(clock), CMD_CLOCK, wide("Show clock").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_IGNORE, wide("Hidden apps...").as_ptr());
        AppendMenuW(
            menu,
            MF_STRING | check(autostart_enabled()),
            CMD_AUTOSTART,
            wide("Start with Windows").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Exit\tCtrl+Shift+F12").as_ptr());

        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd.max(0) as usize
    }
}

// ------------------------------------------------------------------ settings

/// Boolean setting stored as a DWORD under HKCU\Software\FpsOverlay.
pub fn get_flag(name: &str) -> Option<bool> {
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(SETTINGS_KEY).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut v as *mut u32 as *mut _,
            &mut size,
        )
    };
    (r == 0).then_some(v != 0)
}

pub fn set_flag(name: &str, on: bool) {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(SETTINGS_KEY).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        ) != 0
        {
            return;
        }
        let v = on as u32;
        RegSetValueExW(key, wide(name).as_ptr(), 0, REG_DWORD, &v as *const u32 as *const u8, 4);
        RegCloseKey(key);
    }
}

/// String setting stored as REG_SZ under HKCU\Software\FpsOverlay.
pub fn get_string(name: &str) -> Option<String> {
    unsafe {
        let key = wide(SETTINGS_KEY);
        let val = wide(name);
        let mut size = 0u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ,
            null_mut(), null_mut(), &mut size,
        );
        if r != 0 || size < 2 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        let r = RegGetValueW(
            HKEY_CURRENT_USER, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ,
            null_mut(), buf.as_mut_ptr() as *mut _, &mut size,
        );
        if r != 0 {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

pub fn set_string(name: &str, value: &str) {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER, wide(SETTINGS_KEY).as_ptr(), 0, null(), 0,
            KEY_SET_VALUE, null(), &mut key, null_mut(),
        ) != 0
        {
            return;
        }
        let v = wide(value);
        RegSetValueExW(key, wide(name).as_ptr(), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32);
        RegCloseKey(key);
    }
}

// ------------------------------------------------------------------ autostart

// Autostart goes through a scheduled task that runs elevated ("highest privileges") at
// logon, so Windows starts the overlay as admin without a UAC prompt at every login.
//
// Security: an elevated task is only as safe as the file it launches and the XML it is
// created from, so
//   * the task never points into a user-writable folder: the exe is always run from
//     %ProgramFiles%\SimpleOverlay, a folder this code creates (needs admin);
//   * the task XML is written to %WINDIR%\Temp under a random name with CREATE_NEW
//     (so no pre-created file or symlink is reused). The file is owned by Administrators
//     and gets no write access for the non-elevated token, so it cannot be swapped
//     before schtasks reads it; it is removed right after;
//   * schtasks.exe is started by its full System32 path, and the task's user is taken
//     from the process token (SID), not from environment variables.
// If any of that is not possible (not elevated) the plain HKCU Run entry is used and
// Windows asks for UAC at login as before.
//
// All of it runs on a background thread (schtasks takes 100-300 ms per call); the
// menu reads a cached state.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

use windows_sys::core::GUID;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Per-user task name ("FpsOverlay-<SID>"): two accounts on one PC must not overwrite
/// each other's task.
fn task_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| format!("FpsOverlay-{}", current_user_sid().unwrap_or_default()))
}

const INSTALL_DIR: &str = "SimpleOverlay";
/// FOLDERID_ProgramFiles
const FOLDERID_PROGRAM_FILES: GUID = GUID::from_u128(0x905E63B6_C1BF_494E_B29C_65B732D3D21A);

/// Cached "autostart is on" state shown in the menu.
static AUTOSTART: AtomicBool = AtomicBool::new(false);

fn win_dir(f: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    let mut buf = [0u16; 512];
    let n = unsafe { f(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

fn schtasks(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    let Some(sys) = win_dir(GetSystemDirectoryW) else { return false };
    std::process::Command::new(sys.join("schtasks.exe"))
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", task_name()])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// SID string (S-1-5-21-...) of the user this process runs as.
fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)]; // 8-byte aligned
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 || s.is_null() {
            return None;
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s as _);
        Some(out)
    }
}

/// `%ProgramFiles%\SimpleOverlay` (may not exist).
pub fn install_dir() -> Option<PathBuf> {
    program_files().map(|p| p.join(INSTALL_DIR))
}

/// %ProgramFiles% from the shell's known-folder API (not from an overridable env var).
fn program_files() -> Option<PathBuf> {
    unsafe {
        let mut p: *mut u16 = null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_PROGRAM_FILES, 0, null_mut(), &mut p);
        if hr < 0 || p.is_null() {
            return None;
        }
        let n = (0..).take_while(|&i| *p.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        CoTaskMemFree(p as _);
        Some(PathBuf::from(out))
    }
}

/// The exe the task should launch: always the copy in `%ProgramFiles%\SimpleOverlay`
/// (installed or updated from `exe`). None if that is not possible (not elevated).
fn protected_exe(exe: &Path) -> Option<PathBuf> {
    // Only our own folder is trusted: Program Files subfolders can be user-writable
    // (installers sometimes make them so), so being "somewhere in Program Files" proves
    // nothing. %ProgramFiles%\SimpleOverlay is created here by an elevated process and
    // inherits Program Files' admin-only write permissions.
    let dir = program_files()?.join(INSTALL_DIR);
    let target = dir.join("overlay.exe");
    let lower = |p: &Path| p.display().to_string().to_ascii_lowercase();
    if lower(exe) == lower(&target) {
        return Some(target);
    }
    std::fs::create_dir_all(&dir).ok()?; // fails without admin
    // Install when missing, or when this exe is newer AND different. Never downgrade: an
    // older copy being run must not replace a newer installed one.
    let needs_copy = match (std::fs::metadata(exe), std::fs::metadata(&target)) {
        (Ok(src), Ok(dst)) => src.modified().ok() > dst.modified().ok() && !same_contents(exe, &target),
        _ => true,
    };
    // A failed copy (e.g. the installed copy is running) keeps the existing one.
    if needs_copy && std::fs::copy(exe, &target).is_err() && !target.exists() {
        return None;
    }
    target.exists().then_some(target)
}

fn same_contents(a: &Path, b: &Path) -> bool {
    matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
}

/// Creates (or replaces) the logon task for `exe`. False if that is not permitted.
fn create_task(exe: &Path, sid: &str) -> bool {
    use std::io::Write;
    let dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{cmd}</Command><WorkingDirectory>{dir}</WorkingDirectory></Exec></Actions>
</Task>"#,
        sid = xml_escape(sid),
        cmd = xml_escape(&exe.display().to_string()),
        dir = xml_escape(&dir),
    );
    // schtasks wants UTF-16 with BOM.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|c| c.to_le_bytes()));

    // %WINDIR%\Temp: standard users may create files there, but a file created by this
    // elevated process is not writable by them afterwards; CREATE_NEW + an unpredictable
    // name rule out pre-created files, hard links and symlinks.
    let Some(win) = win_dir(GetWindowsDirectoryW) else { return false };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = win.join("Temp").join(format!("simple-overlay-{}-{nanos}.xml", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(&bytes));
    if written.is_err() {
        let _ = std::fs::remove_file(&path);
        return false;
    }
    let ok = schtasks(&["/Create", "/TN", task_name(), "/XML", &path.display().to_string(), "/F"]);
    let _ = std::fs::remove_file(&path);
    ok
}

/// Turns autostart on: elevated task if possible, Run entry otherwise.
fn enable_autostart() {
    let task_ok = (|| {
        let exe = std::env::current_exe().ok()?;
        let target = protected_exe(&exe)?;
        Some(create_task(&target, &current_user_sid()?))
    })()
    .unwrap_or(false);
    if task_ok {
        set_run_key(false); // the task replaces the Run entry
    } else if !task_exists() {
        set_run_key(true); // not allowed to create the task: fall back (UAC at login)
    }
}

fn disable_autostart() {
    if task_exists() {
        schtasks(&["/Delete", "/TN", task_name(), "/F"]);
        if task_exists() {
            // Deleting needs the same admin rights as creating.
            unsafe {
                MessageBoxW(
                    null_mut(),
                    wide("The autostart task could not be removed.\nRun FPS Overlay as administrator and turn off \"Start with Windows\" again.").as_ptr(),
                    wide("FPS Overlay").as_ptr(),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            return;
        }
        // Best effort: remove the installed copy (fails while it is the running exe).
        if let Some(pf) = program_files() {
            let dir = pf.join(INSTALL_DIR);
            let _ = std::fs::remove_file(dir.join("overlay.exe"));
            let _ = std::fs::remove_dir(&dir);
        }
    }
    set_run_key(false);
}

fn query_autostart() -> bool {
    task_exists() || run_key_enabled()
}

/// Cached state (cheap, safe to call from the UI thread).
pub fn autostart_enabled() -> bool {
    AUTOSTART.load(Ordering::Relaxed)
}

/// All autostart work runs under this lock, one operation at a time, so a toggle can
/// never interleave with the startup refresh or another toggle.
static AUTOSTART_LOCK: Mutex<()> = Mutex::new(());
/// What the user last asked for: 0 = nothing yet, 1 = on, 2 = off. Workers act on the
/// latest request, not on the one they were spawned for.
static WANT: AtomicU8 = AtomicU8::new(0);

pub fn set_autostart(on: bool) {
    AUTOSTART.store(on, Ordering::Relaxed);
    WANT.store(if on { 1 } else { 2 }, Ordering::SeqCst);
    std::thread::spawn(|| {
        let _guard = AUTOSTART_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match WANT.load(Ordering::SeqCst) {
            1 => enable_autostart(),
            2 => disable_autostart(),
            _ => {}
        }
        AUTOSTART.store(query_autostart(), Ordering::Relaxed);
    });
}

/// At startup: read the real state, and if autostart is on rewrite the entry (follows an
/// updated exe, and upgrades an old Run entry to the elevated task when running as admin).
pub fn refresh_autostart() {
    std::thread::spawn(|| {
        let _guard = AUTOSTART_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if WANT.load(Ordering::SeqCst) != 0 {
            return; // the user already chose something; that choice wins
        }
        let on = query_autostart();
        AUTOSTART.store(on, Ordering::Relaxed);
        if on {
            enable_autostart();
            AUTOSTART.store(query_autostart(), Ordering::Relaxed);
        }
    });
}

fn run_key_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(RUN_VALUE).as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            null_mut(),
        ) == 0
    }
}

fn set_run_key(on: bool) {
    unsafe {
        let mut key: HKEY = null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key) != 0 {
            return;
        }
        let name = wide(RUN_VALUE);
        if on {
            let Ok(exe) = std::env::current_exe() else {
                RegCloseKey(key);
                return;
            };
            let cmd = wide(&format!("\"{}\"", exe.display()));
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                cmd.as_ptr() as *const u8,
                (cmd.len() * 2) as u32,
            );
        } else {
            RegDeleteValueW(key, name.as_ptr());
        }
        RegCloseKey(key);
    }
}

